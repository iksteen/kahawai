import { useEffect, useRef } from 'react'
import type JASSUB from 'jassub'
import {
  fetchFonts,
  fontUrl,
  isRasterSub,
  overlayUrl,
  subtitleFileUrl,
  type Session,
  type Subtitle,
} from './api'
import { loadChunk } from './chunk'
import { playerNote } from './player-note'
import type { SubtitleRoute } from './subtitle-route'

/// The three ways a subtitle track reaches the screen, and the one that keeps a
/// native <track> in the right mode.
///
/// Lifted out of the player whole: these talk to the video element and to their
/// own canvases, and to nothing else in it. What they need from the component
/// is the element, the chosen track and where the current run starts.
export function useSubtitleRenderers(p: {
  videoRef: React.RefObject<HTMLVideoElement | null>
  route: SubtitleRoute
  selected: Subtitle | undefined
  subKey: string
  trackEpoch: number
  /// Where the current pipeline run begins, in absolute film ms. Cue times are
  /// absolute, the element's clock is not, so every path shifts by -offset.
  offsetRef: React.RefObject<number>
  item: { id: string }
  session: Session
  isHls: boolean
  /// The live tap yielded nothing: use the flattened .vtt for this track.
  ///
  /// One-way, and no longer a boolean. Clearing is the tracks reducer's job —
  /// `subtitle-chosen` resets the flag, which is exactly the transition that
  /// changes `subKey` — so the `false` this used to accept had no caller and no
  /// effect. It also had a cost: an arrow of the shape `(v) => v && send(...)`
  /// returns a boolean, and React takes an effect's return value as its
  /// cleanup and tries to call it. That broke the screen once already, and the
  /// only thing keeping it from happening again was a comment.
  onTapEmpty: () => void
}) {
  const { videoRef, route, selected, subKey, trackEpoch, offsetRef, item, session, isHls } = p
  /// libass, kept across cue updates so a track switch does not rebuild the
  /// renderer — it is the expensive one.
  const jassubRef = useRef<JASSUB | null>(null)
  const liveText = route === 'live-text'
  const useImage = route === 'image'
  const useAss = route === 'ass'
  const { onTapEmpty } = p

  // A <track> is lazy about mode; force the chosen one to display.
  useEffect(() => {
    const tracks = videoRef.current?.textTracks
    if (!tracks) return
    for (const t of Array.from(tracks)) t.mode = subKey ? 'showing' : 'disabled'
    // A ref arriving as a prop is not recognised as one, so the rule asks for
    // `.current` in the deps, where it would do nothing but churn.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [subKey, trackEpoch])

  const jsTrackRef = useRef<TextTrack | null>(null)

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
      const resp = await fetch(`${base}subs-${selected.id}.jsonl`, { signal: ac.signal })
      if (!resp.ok || !resp.body) {
        onTapEmpty()
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
      if (!got && !dead) onTapEmpty()
    })().catch(() => {
      if (!dead) onTapEmpty()
    })
    return () => {
      dead = true
      ac.abort()
      clear()
      track.mode = 'disabled'
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [liveText, subKey, trackEpoch, item.id])

  const imgCanvasRef = useRef<HTMLCanvasElement | null>(null)
  useEffect(() => {
    const video = videoRef.current
    if (!video || !useImage || !selected) return
    if (!isHls && !isRasterSub(selected)) return
    let dead = false
    const ac = new AbortController()
    type ImgSet = {
      s: number
      cw: number
      ch: number
      objects: { x: number; y: number; img: ImageBitmap }[]
    }
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
        // The canvas backs the FRAME, and composition space maps onto
        // it UNIFORMLY by width (mirror of the burn path's compose():
        // a 3840x1600 scope film carries subs authored against
        // 1920x1080, and stretching the composition canvas to the
        // content box scaled the axes independently — text 1.35x too
        // wide vs burn-in/mpv). Bottom-anchored subs on a canvas
        // taller than the picture clamp back on screen, like burn.
        const vw = video.videoWidth || 1920
        const vh = video.videoHeight || 1080
        canvas.width = vw
        canvas.height = vh
        const ctx = canvas.getContext('2d')!
        ctx.clearRect(0, 0, vw, vh)
        if (set) {
          const scale = set.cw > 0 ? vw / set.cw : 1
          for (const o of set.objects) {
            const w = Math.max(1, Math.round(o.img.width * scale))
            const h = Math.max(1, Math.round(o.img.height * scale))
            const x = Math.max(0, Math.min(Math.round(o.x * scale), vw - w))
            const y = Math.max(0, Math.min(Math.round(o.y * scale), vh - h))
            ctx.drawImage(o.img, x, y, w, h)
          }
        }
      }
      place()
    }
    // Interval-driven (not rVFC): keeps drawing with the tab occluded,
    // and 200 ms is well inside PGS timing tolerance.
    const timer = window.setInterval(draw, 200)
    video.addEventListener('seeked', draw)

    ;(async () => {
      const url = overlayUrl(selected, item.id, session.stream_url)
      const resp = await fetch(url, { signal: ac.signal })
      // Reported for the same reason as the ASS feed below: `delivery` is
      // `overlay` here, and the `.vtt` <track> renders only for `text`, so
      // there is nothing to fall back to. An image track that fetches nothing
      // is a picture with no subtitles on it and no way to tell that from a
      // track that was always empty.
      if (!resp.ok || !resp.body) {
        if (!dead) playerNote('Subtitles for this track could not be loaded.')
        return
      }
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
    })().catch(() => {
      // Aborts are ordinary and set `dead` first; anything else lost the track.
      if (!dead) playerNote('Subtitles for this track could not be loaded.')
    })

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
        fonts = f.fonts.map((_, i) => fontUrl(item.id, i))
      } catch {
        /* no fonts — libass falls back */
      }
      if (dead) return
      // UX-4: libass is a worker plus a couple of megabytes of wasm, and
      // only a styled track ever needs it. Fetched here, inside the effect
      // that has already decided this track is ASS — not at import time,
      // where it rode along with every session including audio-only ones.
      const { default: JASSUB } = await loadChunk('jassub', () => import('jassub'))
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
      if (isHls && selected.origin === 'embedded') {
        const base = session.stream_url.replace(/[^/]*$/, '')
        fed = await feed(`${base}subs-${selected.id}.ass`).catch(() => false)
      }
      // These are different services — the tap comes off the mediahost through
      // the session, the fallback off the hub — so a tap that dies after its
      // header leaves the fallback perfectly able to answer, and it is the only
      // thing that can complete the script.
      //
      // Known, and deliberately not gated on `instance`: when the tap died
      // AFTER building the renderer, this appends the whole file on top of the
      // cues libass already holds, so the overlap is drawn twice. A gate on
      // `instance` was tried and is worse — it leaves a styled canvas with the
      // handful of cues that arrived and a complete copy sitting on the hub
      // untouched, silently. Doubled text beats missing text; the real fix is
      // for `feed` to be able to replace a renderer rather than only append,
      // which is more than this change should carry.
      if (!fed && !dead) {
        await feed(subtitleFileUrl(item.id, `${selected.id}.ass`)).catch(() => false)
      }
      // There is no fallback on this path, which an earlier comment here
      // claimed there was: the `.vtt` <track> renders only for
      // `delivery === 'text'`, and reaching this effect at all means the hub
      // said `ass`. So a feed that never saw a header is simply no subtitles,
      // and saying nothing left the viewer to conclude the track was empty.
      // `showNote`, not `notify`, because this can happen in fullscreen.
      //
      // On the INSTANCE, not on `fed`. A stream that dies after the header is
      // read has already put subtitles on screen and still returns false, so
      // reporting on `fed` would deny what the viewer can see.
      if (!instance && !dead) playerNote('Subtitles for this track could not be loaded.')
    })().catch(() => {
      // In practice this is the `import('jassub')` above and nothing else: both
      // feeds swallow their own rejections, so an abort mid-read never reaches
      // here. The import is its own chunk, so a tab left open across a hub
      // redeploy asks for a hash the new binary does not embed. `loadChunk`
      // turns that into one reload — which is the only thing that fixes it —
      // so what reaches here is a genuine failure. `dead` still guards it, for
      // an unmount landing between the awaits.
      if (!dead) playerNote('Subtitles for this track could not be loaded.')
    })
    return () => {
      dead = true
      ac.abort()
      instance?.destroy()
      if (jassubRef.current === instance) jassubRef.current = null
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [useAss, subKey, trackEpoch, item.id])

  return {
    /// A seek-restart moves where the run begins, and the epoch bump below
    /// rebuilds the renderer — but not before the next frame, and the skew is
    /// visible until it does. libass can be told directly.
    nudgeOffset: (absMs: number) => {
      if (jassubRef.current) jassubRef.current.timeOffset = absMs / 1000
    },
  }
}
