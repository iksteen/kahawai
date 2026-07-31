import { useEffect, useRef, useState } from 'react'
import Hls from 'hls.js'
import JASSUB from 'jassub'
import {
  accessToken,
  api,
  endSession,
  fetchFonts,
  fetchItem,
  fetchLibraries,
  fetchPrefs,
  fetchSubtitles,
  pickSubtitle,
  postProgress,
  putPref,
  resolveTracks,
  seekSession,
  startPlaybackSession,
  subtitleLabel,
  type ItemDetail,
  type Session,
  type Subtitle,
} from '../api'
import { buildProfile, loadMask, maskSummary } from '../capabilities'
import CapabilityDebug from './CapabilityDebug'

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
  libraryId,
  onClose,
  onRestart,
}: {
  item: ItemDetail
  session: Session
  resumeMs: number
  libraryId: string
  onClose: () => void
  /** Play again from `at` on a freshly negotiated session (capability
   *  debug: a mask only takes effect on a new session). */
  onRestart: (session: Session, at: number) => void
}) {
  const videoRef = useRef<HTMLVideoElement>(null)
  // The capabilities THIS session was negotiated with: frozen at mount
  // so the player's own rendering can never disagree with what the hub
  // was told (a mask edited mid-session applies on the next restart).
  const capsRef = useRef(buildProfile())
  const [showCaps, setShowCaps] = useState(false)
  const [restarting, setRestarting] = useState(false)
  const maskedRef = useRef(maskSummary(loadMask()))
  const hlsRef = useRef<Hls | null>(null)
  const isHls = session.stream_url.endsWith('.m3u8')
  // For HLS sessions the pipeline itself starts at resumeMs, so the
  // playlist's t=0 is that offset; direct sessions play the real file.
  const offsetRef = useRef(isHls ? resumeMs : 0)
  // Multi-part sources: the pipeline's start.pos is local to its part;
  // the absolute timeline origin is partBase + start.pos.
  const partBaseRef = useRef(session.part_base_ms ?? 0)
  const durationMs = session.duration_ms ?? 0
  const [posMs, setPosMs] = useState(offsetRef.current)
  const [seeking, setSeeking] = useState(false)
  const [subs, setSubs] = useState<Subtitle[]>([])
  const [subKey, setSubKey] = useState('')
  // HUB-33: memory scope for manual track choices (series id, or the
  // item itself for movies).
  const seriesRef = useRef<string>(item.id)
  const jassubRef = useRef<JASSUB | null>(null)
  const [audioTracks, setAudioTracks] = useState<
    { codec: string; channels: number; language?: string | null }[]
  >([])
  const [audioTrack, setAudioTrack] = useState(0)
  // Ordered subtitle-language wishlist; [] → subtitles stay off.
  const subsPrefRef = useRef<string[]>([])
  const [videoTracks, setVideoTracks] = useState<
    { codec: string; width: number; height: number }[]
  >([])
  const [videoTrack, setVideoTrack] = useState(0)
  // The <track> URL must shift cues when the HLS timeline starts mid-file;
  // bump on seek-restarts so the track reloads with the new shift.
  const [trackEpoch, setTrackEpoch] = useState(0)
  // Live stream verdicts: a track switch re-plans server-side and the
  // overlay must describe what plays NOW.
  const [streams, setStreams] = useState(session.streams)

  useEffect(() => {
    // One resolution (HUB-33), same helper Detail used to start the
    // session: prefs + streams → selector state and subtitle default.
    Promise.all([
      fetchItem(item.id),
      fetchPrefs().catch(() => ({ prefs: [] })),
      fetchLibraries().catch(() => ({ libraries: [] })),
    ])
      .then(([d, p, l]) => {
        seriesRef.current = d.parent_id ?? item.id
        const mediaType = l.libraries.find((x) => x.id === libraryId)?.media_type ?? ''
        const audio = d.sources_detail[0]?.streams?.audio ?? []
        setAudioTracks(audio)
        setVideoTracks(d.sources_detail[0]?.streams?.video ?? [])
        const r = resolveTracks(
          p.prefs,
          seriesRef.current,
          item.id,
          mediaType,
          d.metadata?.original_language,
          audio,
        )
        setAudioTrack(r.audioTrack)
        subsPrefRef.current = r.subs
        // HUB-32b: a client that declares no display-set compositing
        // is not offered image subtitles at all.
        return fetchSubtitles(item.id, capsRef.current.graphics_overlay)
      })
      .then((r) => {
        setSubs(r.subtitles)
        // Auto-pick the first wishlist match; never overrides a choice.
        const pick = pickSubtitle(subsPrefRef.current, r.subtitles)
        if (pick) setSubKey((cur) => cur || pick.key)
      })
      .catch(() => {
        setSubs([])
        setAudioTracks([])
        setVideoTracks([])
      })
  }, [item.id, libraryId])

  // A capability mask reaches the hub only on a NEW session — the hub
  // stores the effective profile per session and re-plans track
  // switches against it — so applying one restarts playback here.
  const [capsError, setCapsError] = useState('')
  const restartWithCaps = async () => {
    setRestarting(true)
    setCapsError('')
    try {
      const at = Math.round(posMs)
      const fresh = await startPlaybackSession(item, at, audioTrack, videoTrack)
      onRestart(fresh, at) // remounts this component; the old session ends in cleanup
    } catch (e) {
      setCapsError(String(e))
      setRestarting(false)
    }
  }

  // Track switching is a seek-restart at the current position with the
  // new track (§6 machinery; ~2 s hiccup, same as a deep seek).
  const switchTracks = async (audio: number, video_: number) => {
    const video = videoRef.current!
    if (audio !== audioTrack) {
      // Two additive layers (HUB-33). The SERIES remembers the
      // language (portable across episodes with differing track
      // orders) — a language-motivated switch keeps steering the whole
      // series. MOVIES additionally pin the exact track index: "the
      // commentary track of THIS film" has no language representation,
      // and there is no series intent to follow. Episodes deliberately
      // do NOT pin, so one episode never freezes on an old choice.
      const value = audioTracks[audio]?.language?.toLowerCase() ?? `#${audio}`
      void putPref(seriesRef.current, 'audio', value).catch(() => {})
      if (item.kind === 'movie') {
        void putPref(item.id, 'audio.track', `#${audio}`).catch(() => {})
      }
    }
    setAudioTrack(audio)
    setVideoTrack(video_)
    setSeeking(true)
    hlsRef.current?.stopLoad() // the restart 404s the old run's segments
    video.pause()
    try {
      const absMs = offsetRef.current + video.currentTime * 1000
      const r = await seekSession(session.session_id, absMs, audio, video_)
      if (r.streams) setStreams(r.streams)
      partBaseRef.current = r.part_base_ms ?? 0
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

  const selected = subs.find((s) => s.key === subKey)
  const isAss = !!selected && (selected.format === 'ass' || selected.format === 'ssa')
  // HUB-32a: faithful ASS rendering only when this client DECLARES it.
  // Without the declaration the hub plans the flattened-to-VTT tier and
  // the player must actually take it — otherwise a masked run would
  // change the verdict text and nothing else.
  const useAss = isAss && capsRef.current.ass_render
  // Embedded non-ASS on an HLS session rides the live session tap and
  // feeds cues via the TextTrack API (a <track> can't consume a
  // growing document). Falls back to the item-level .vtt <track> when
  // the tap yields nothing (old satellite, no pipeline).
  const [vttFallback, setVttFallback] = useState(false)
  // Sidecar image tracks (.idx/.sub) have no session tap to feed the
  // overlay; their serving path is the OCR text row (HUB-32c).
  const useImage = !!selected && !!selected.image && selected.kind === 'embedded'
  // Keyed on the FORMAT, not on useAss: the pipeline taps an ASS track
  // as .ass and never writes the .jsonl this path reads, so a client
  // that declined ASS rendering must go straight to the flattened
  // .vtt track rather than chase a tap that cannot exist.
  const liveText =
    isHls &&
    !!selected &&
    !isAss &&
    !useImage &&
    selected.kind === 'embedded' &&
    !vttFallback
  const jsTrackRef = useRef<TextTrack | null>(null)

  useEffect(() => setVttFallback(false), [subKey])

  // Live cue feed for embedded non-ASS tracks: tail the session's
  // .jsonl tap and append cues to a JS-managed TextTrack. Cue times
  // are absolute file ms; the video timeline starts at the playlist
  // origin, so cues shift by -offset.
  useEffect(() => {
    const video = videoRef.current
    if (!video || !liveText || !selected) return
    let dead = false
    const ac = new AbortController()
    if (!jsTrackRef.current) {
      // addTextTrack is permanent on the element; create once, reuse.
      jsTrackRef.current = video.addTextTrack('subtitles', 'kahawai', '')
    }
    const track = jsTrackRef.current
    track.mode = 'showing'
    const clear = () => {
      while (track.cues && track.cues.length > 0) track.removeCue(track.cues[0])
    }
    clear()
    ;(async () => {
      const base = session.stream_url.replace(/[^/]*$/, '')
      const resp = await fetch(`${base}subs-${selected.key}.jsonl`, { signal: ac.signal })
      if (!resp.ok || !resp.body) {
        setVttFallback(true)
        return
      }
      const reader = resp.body.getReader()
      const dec = new TextDecoder()
      let buf = ''
      let got = false
      for (;;) {
        const { done, value } = await reader.read()
        if (dead) return
        if (value) buf += dec.decode(value, { stream: true })
        const cut = buf.lastIndexOf('\n')
        if (cut >= 0) {
          for (const line of buf.slice(0, cut).split('\n')) {
            if (!line.trim()) continue
            try {
              const c = JSON.parse(line)
              const off = offsetRef.current
              track.addCue(new VTTCue((c.s - off) / 1000, (c.e - off) / 1000, c.t))
              got = true
            } catch {
              /* partial or malformed line */
            }
          }
          buf = buf.slice(cut + 1)
        }
        if (done) break
      }
      if (!got && !dead) setVttFallback(true)
    })().catch(() => {
      if (!dead) setVttFallback(true)
    })
    return () => {
      dead = true
      ac.abort()
      clear()
      track.mode = 'disabled'
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [liveText, subKey, trackEpoch, item.id])

  // Image subtitles (PGS/VobSub): the session tap streams decoded
  // display sets — {"s",cw,ch,"o":[{x,y,png}]} — and we draw the
  // latest set at/before the playhead on a canvas overlay, scaled from
  // composition space to the video's displayed content box.
  const imgCanvasRef = useRef<HTMLCanvasElement | null>(null)
  useEffect(() => {
    const video = videoRef.current
    if (!video || !useImage || !selected || !isHls) return
    let dead = false
    const ac = new AbortController()
    type ImgSet = { s: number; cw: number; ch: number; objects: { x: number; y: number; img: ImageBitmap }[] }
    const sets: ImgSet[] = []
    let drawnIdx = -1

    if (!imgCanvasRef.current) {
      const c = document.createElement('canvas')
      c.className = 'imgsub-canvas'
      c.style.position = 'absolute'
      c.style.pointerEvents = 'none'
      video.insertAdjacentElement('afterend', c)
      imgCanvasRef.current = c
    }
    const canvas = imgCanvasRef.current
    canvas.style.display = 'block'

    const place = () => {
      // The displayed content box: aspect-fit inside the element.
      const vw = video.videoWidth || 16
      const vh = video.videoHeight || 9
      const er = video.clientWidth / video.clientHeight
      const cr = vw / vh
      const w = er > cr ? video.clientHeight * cr : video.clientWidth
      const h = er > cr ? video.clientHeight : video.clientWidth / cr
      canvas.style.width = `${Math.round(w)}px`
      canvas.style.height = `${Math.round(h)}px`
      canvas.style.left = `${Math.round(video.offsetLeft + (video.clientWidth - w) / 2)}px`
      canvas.style.top = `${Math.round(video.offsetTop + (video.clientHeight - h) / 2)}px`
    }

    const draw = () => {
      if (dead) return
      const t = video.currentTime * 1000 + offsetRef.current
      let idx = -1
      for (let i = sets.length - 1; i >= 0; i--) {
        if (sets[i].s <= t) {
          idx = i
          break
        }
      }
      if (idx !== drawnIdx) {
        drawnIdx = idx
        const set = idx >= 0 ? sets[idx] : null
        canvas.width = set?.cw || 1920
        canvas.height = set?.ch || 1080
        const ctx = canvas.getContext('2d')!
        ctx.clearRect(0, 0, canvas.width, canvas.height)
        if (set) for (const o of set.objects) ctx.drawImage(o.img, o.x, o.y)
      }
      place()
    }
    // Interval-driven (not rVFC): keeps drawing with the tab occluded,
    // and 200 ms is well inside PGS timing tolerance.
    const timer = window.setInterval(draw, 200)
    video.addEventListener('seeked', draw)

    ;(async () => {
      const base = session.stream_url.replace(/[^/]*$/, '')
      const resp = await fetch(`${base}subs-${selected.key}.jsonl`, { signal: ac.signal })
      if (!resp.ok || !resp.body) return
      const reader = resp.body.getReader()
      const dec = new TextDecoder()
      let buf = ''
      for (;;) {
        const { done, value } = await reader.read()
        if (dead) return
        if (value) buf += dec.decode(value, { stream: true })
        const cut = buf.lastIndexOf('\n')
        if (cut >= 0) {
          for (const line of buf.slice(0, cut).split('\n')) {
            if (!line.trim()) continue
            try {
              const j = JSON.parse(line)
              const objects = await Promise.all(
                (j.o as { x: number; y: number; png: string }[]).map(async (o) => {
                  const bytes = Uint8Array.from(atob(o.png), (ch) => ch.charCodeAt(0))
                  const img = await createImageBitmap(new Blob([bytes], { type: 'image/png' }))
                  return { x: o.x, y: o.y, img }
                }),
              )
              sets.push({ s: j.s, cw: j.cw, ch: j.ch, objects })
              drawnIdx = -2 // force redraw check
            } catch {
              /* partial line */
            }
          }
          buf = buf.slice(cut + 1)
        }
        if (done) break
      }
    })().catch(() => {})

    return () => {
      dead = true
      ac.abort()
      window.clearInterval(timer)
      video.removeEventListener('seeked', draw)
      canvas.style.display = 'none'
      const ctx = canvas.getContext('2d')
      ctx?.clearRect(0, 0, canvas.width, canvas.height)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [useImage, subKey, trackEpoch, item.id])

  // Faithful ASS rendering (HUB-32): JASSUB draws with libass on a
  // canvas over the video, fed the original script and the source's
  // embedded fonts. ASS times are absolute file times; timeOffset
  // bridges to the (possibly mid-file) HLS timeline. Re-created on
  // seek-restarts (trackEpoch) to pick up the new offset.
  //
  // The .ass endpoint STREAMS on first extraction (header, then
  // Dialogue lines as the demux pass reaches them): the instance is
  // created as soon as the header is in and later lines feed libass
  // incrementally — no waiting out a full-file read.
  useEffect(() => {
    const video = videoRef.current
    if (!video || !useAss || !selected) return
    let dead = false
    let instance: JASSUB | null = null
    const ac = new AbortController()
    ;(async () => {
      let fonts: string[] = []
      try {
        const f = await fetchFonts(item.id)
        fonts = f.fonts.map((_, i) => `/api/v1/items/${item.id}/fonts/${i}`)
      } catch {
        /* no fonts — libass falls back */
      }
      if (dead) return
      // Feed JASSUB from a (possibly growing) ASS stream. Returns true
      // once the header was seen — i.e. the source had the track.
      const feed = async (url: string): Promise<boolean> => {
        const resp = await fetch(url, { signal: ac.signal })
        if (!resp.ok || !resp.body) return false
        const reader = resp.body.getReader()
        const dec = new TextDecoder()
        let buf = ''
        for (;;) {
          const { done, value } = await reader.read()
          if (dead) return true
          if (value) buf += dec.decode(value, { stream: true })
          if (!instance) {
            // Wait for the complete header: everything up to and
            // including the [Events] Format line.
            const ev = buf.toLowerCase().indexOf('[events]')
            const fm = ev >= 0 ? buf.indexOf('Format:', ev) : -1
            const nl = fm >= 0 ? buf.indexOf('\n', fm) : -1
            if (nl >= 0) {
              instance = new JASSUB({
                video,
                subContent: buf.slice(0, nl + 1),
                fonts,
                timeOffset: offsetRef.current / 1000,
              })
              jassubRef.current = instance
              buf = buf.slice(nl + 1)
            }
          }
          if (instance && buf) {
            const cut = buf.lastIndexOf('\n')
            if (cut >= 0) {
              const lines = buf.slice(0, cut + 1)
              buf = buf.slice(cut + 1)
              await instance.ready
              if (dead) return true
              // renderer is the worker proxy; processData appends events.
              // The section header keeps libass's line parser in [Events]
              // regardless of its state after the initial track load.
              void (instance as any).renderer.processData('[Events]\n' + lines)
            }
          }
          if (done) return instance != null
        }
      }
      // The remux session taps embedded ASS live from the playback
      // origin — subtitles for what you're watching, instantly, at any
      // position. Sidecars and non-HLS fall back to the item endpoint
      // (whole-file extraction, streamed).
      let fed = false
      if (isHls && selected.key.startsWith('e')) {
        const base = session.stream_url.replace(/[^/]*$/, '')
        fed = await feed(`${base}subs-${selected.key}.ass`).catch(() => false)
      }
      if (!fed && !dead) {
        await feed(`/api/v1/items/${item.id}/subtitles/${selected.key}.ass`).catch(() => {})
      }
    })().catch(() => {
      /* aborted or stream error; VTT fallback stays available */
    })
    return () => {
      dead = true
      ac.abort()
      instance?.destroy()
      if (jassubRef.current === instance) jassubRef.current = null
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [useAss, subKey, trackEpoch, item.id])

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
          const local = Math.round(Number(await r.text()))
          const n = partBaseRef.current + local
          if (
            Number.isFinite(n) &&
            n !== offsetRef.current &&
            Math.abs(n - offsetRef.current) < 60000
          ) {
            const video = videoRef.current
            offsetRef.current = n
            // Correct a live ASS renderer immediately — the epoch bump
            // recreates it, but the skew shouldn't be visible meanwhile.
            if (jassubRef.current) jassubRef.current.timeOffset = n / 1000
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
    // The restart replaces the run server-side: every not-yet-fetched
    // segment of the OLD run is about to 404. Stop loading and freeze
    // the picture so the wait is visible instead of the player merrily
    // playing on while spraying 404s in the console.
    hlsRef.current?.stopLoad()
    video.pause()
    try {
      const r = await seekSession(session.session_id, targetMs)
      partBaseRef.current = r.part_base_ms ?? 0
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
    const onEnded = () => {
      report()
      // Multi-part sources (CD1/CD2): this part's playlist ended but
      // the film hasn't — restart into the next part.
      if (isHls && (session.parts ?? 1) > 1 && durationMs > 0) {
        const abs = absMs()
        if (abs < durationMs - 3000) void seekTo(abs + 250)
      }
    }
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
    // Everything this effect reads is fixed for the session: durationMs
    // and isHls are derived from the `session` prop, parts/resumeMs are
    // props, and attach/seekTo close over only those plus refs. A new
    // session REMOUNTS this component (App renders it keyed on
    // session_id), so none of it can go stale here. Listing them would
    // re-run the cleanup — which reports progress, ends the session and
    // destroys the hls instance — on every unrelated render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.session_id])

  const pct = durationMs > 0 ? Math.min(100, (posMs / durationMs) * 100) : 0
  return (
    <main className="player">
      <button className="btn ghost small" onClick={onClose}>
        ← Back
      </button>
      <div className="videobox">
        <video
          ref={videoRef}
          controls
          controlsList="nofullscreen"
          playsInline
          crossOrigin="use-credentials"
        >
          {subKey && !useAss && !liveText && !useImage && (
            <track
              key={`${subKey}-${trackEpoch}`}
              default
              kind="subtitles"
              src={`/api/v1/items/${item.id}/subtitles/${subKey}.vtt?shift_ms=${-Math.round(offsetRef.current)}`}
            />
          )}
        </video>
        {seeking && (
          <div className="seek-veil" aria-label="Restarting stream">
            <span className="seek-veil-spin">&#10227;</span>
          </div>
        )}
        {/* Native fullscreen would take only the <video>, stranding the
            JASSUB canvas; this fullscreens the box holding both. */}
        <button
          className="btn ghost small fs-btn"
          title="Fullscreen"
          onClick={(e) => {
            const box = e.currentTarget.parentElement!
            if (document.fullscreenElement) void document.exitFullscreen()
            else void box.requestFullscreen()
          }}
        >
          ⛶
        </button>
      </div>
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
          <select
            value={subKey}
            onChange={(e) => {
              const key = e.target.value
              setSubKey(key)
              // Remember the explicit choice for this series (HUB-33).
              const s = subs.find((x) => x.key === key)
              const value = key === '' ? 'off' : (s?.language ?? 'any').toLowerCase()
              void putPref(seriesRef.current, 'subs', value).catch(() => {})
            }}
          >
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
        {streams ? ` · video: ${streams.video} · audio: ${streams.audio}` : ''}{' '}
        · {session.content_type}{' '}
        <button className="btn ghost small" onClick={() => setShowCaps((v) => !v)}>
          {showCaps ? 'hide caps' : 'caps'}
        </button>
        {/* The mask this session was negotiated with, always visible:
            a forgotten mask must never read as a bug in the hub. */}
        {maskedRef.current.length > 0 && (
          <span className="caps-badge">masked: {maskedRef.current.join(' ')}</span>
        )}
        {streams?.subtitles?.length ? (
          <div className="dim">
            {streams.subtitles
              .map(
                (s) =>
                  `subs ${s.format}${s.language ? `/${s.language}` : ''}: ${s.tier}` +
                  (s.note ? ` (${s.note})` : ''),
              )
              .join(' · ')}
          </div>
        ) : null}
        {showCaps && <CapabilityDebug onApply={restartWithCaps} applying={restarting} />}
        {capsError && <div className="dim">restart failed: {capsError}</div>}
      </div>
    </main>
  )
}
