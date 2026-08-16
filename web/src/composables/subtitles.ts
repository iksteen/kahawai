/// The three ways a subtitle track reaches the screen, and the one that keeps a
/// native `<track>` in the right mode.
///
/// Kept out of the player: these talk to the video element and to their own
/// canvases, and to nothing else in it. What they need from the component is
/// the element, the chosen track, and where the current run begins.

import { onScopeDispose, type Ref, watch } from 'vue'
import type JASSUB from 'jassub'

import type { StartSessionResponse } from '../api/generated/model/startSessionResponse.ts'
import type { SubtitleRoute } from '../domain/subtitle-route.ts'
import type { TrackListing } from '../api/generated/model/trackListing.ts'
import { fontUrl, overlayUrl, sessionFileUrl, subtitleFileUrl } from '../api/playback.ts'
import { isRasterSub } from '../domain/subtitles.ts'
import { itemFonts } from '../api/generated/kahawai.ts'
import { loadChunk } from '../api/chunk.ts'
import { playerNote } from './player-note.ts'

/// How often the image-subtitle canvas is redrawn. Interval-driven rather than
/// `requestVideoFrameCallback`: it keeps drawing with the tab occluded, and
/// 200 ms is well inside PGS timing tolerance.
const DRAW_MS = 200

export function useSubtitleRenderers(p: {
  video: Ref<HTMLVideoElement | null>
  route: Ref<SubtitleRoute>
  selected: Ref<TrackListing | undefined>
  subKey: Ref<string>
  /// Bumped whenever the run's origin moves, so every renderer rebuilds with
  /// the new cue shift.
  epoch: Ref<number>
  /// Where the current pipeline run begins, in absolute film ms. Cue times are
  /// absolute and the element's clock is not, so every path shifts by −offset.
  offset: Ref<number>
  itemId: Ref<string>
  session: Ref<StartSessionResponse>
  isHls: Ref<boolean>
  /// The live tap yielded nothing: use the flattened .vtt for this track. One
  /// way — clearing it belongs to whatever changes the chosen track.
  onTapEmpty: () => void
}) {
  /// libass, kept across cue updates so a track switch does not rebuild the
  /// renderer. It is the expensive one.
  let jassub: JASSUB | null = null

  // A <track> is lazy about mode; force the chosen one to display.
  watch(
    [p.subKey, p.epoch],
    () => {
      const tracks = p.video.value?.textTracks
      if (!tracks) return
      for (const track of Array.from(tracks)) {
        track.mode = p.subKey.value ? 'showing' : 'disabled'
      }
    },
    { flush: 'post' },
  )

  /// Live cue feed for embedded non-ASS tracks: tail the session's .jsonl tap
  /// and append cues to a JS-managed TextTrack.
  let jsTrack: TextTrack | null = null
  let liveDead: (() => void) | null = null

  function liveText() {
    const video = p.video.value
    const selected = p.selected.value
    if (!video || p.route.value !== 'live-text' || !selected) return
    let dead = false
    const aborter = new AbortController()
    // `addTextTrack` is permanent on the element: create once, reuse.
    jsTrack ??= video.addTextTrack('subtitles', 'kahawai', '')
    const track = jsTrack
    track.mode = 'showing'
    const clear = () => {
      while (track.cues && track.cues.length > 0) track.removeCue(track.cues[0]!)
    }
    clear()

    void (async () => {
      const response = await fetch(
        sessionFileUrl(p.session.value.stream_url, `subs-${selected.id}.jsonl`),
        {
          signal: aborter.signal,
        },
      )
      if (!response.ok || !response.body) {
        p.onTapEmpty()
        return
      }
      const reader = response.body.getReader()
      const decoder = new TextDecoder()
      let buffer = ''
      let got = false
      for (;;) {
        const { done, value } = await reader.read()
        if (dead) return
        if (value) buffer += decoder.decode(value, { stream: true })
        const cut = buffer.lastIndexOf('\n')
        if (cut >= 0) {
          for (const line of buffer.slice(0, cut).split('\n')) {
            if (!line.trim()) continue
            try {
              const cue = JSON.parse(line) as { s: number; e: number; t: string }
              const shift = p.offset.value
              track.addCue(new VTTCue((cue.s - shift) / 1000, (cue.e - shift) / 1000, cue.t))
              got = true
            } catch {
              // A partial or malformed line: the next read completes it.
            }
          }
          buffer = buffer.slice(cut + 1)
        }
        if (done) break
      }
      if (!got && !dead) p.onTapEmpty()
    })().catch(() => {
      if (!dead) p.onTapEmpty()
    })

    liveDead = () => {
      dead = true
      aborter.abort()
      clear()
      track.mode = 'disabled'
    }
  }

  /// Bitmap display sets (PGS, VobSub, and HUB-32d's rasterised ASS), drawn on
  /// a canvas over the picture.
  let canvas: HTMLCanvasElement | null = null
  let imageDead: (() => void) | null = null

  function imageSubs() {
    const video = p.video.value
    const selected = p.selected.value
    if (!video || p.route.value !== 'image' || !selected) return
    if (!p.isHls.value && !isRasterSub(selected)) return
    let dead = false
    const aborter = new AbortController()
    type Displayed = {
      s: number
      cw: number
      ch: number
      objects: { x: number; y: number; img: ImageBitmap }[]
    }
    const sets: Displayed[] = []
    let drawn = -1

    if (!canvas) {
      canvas = document.createElement('canvas')
      canvas.className = 'imgsub-canvas'
      canvas.style.position = 'absolute'
      canvas.style.pointerEvents = 'none'
      video.insertAdjacentElement('afterend', canvas)
    }
    const box = canvas
    box.style.display = 'block'

    /// The displayed content box: aspect-fit inside the element.
    const place = () => {
      const vw = video.videoWidth || 16
      const vh = video.videoHeight || 9
      const elementRatio = video.clientWidth / video.clientHeight
      const contentRatio = vw / vh
      const w = elementRatio > contentRatio ? video.clientHeight * contentRatio : video.clientWidth
      const h = elementRatio > contentRatio ? video.clientHeight : video.clientWidth / contentRatio
      box.style.width = `${Math.round(w)}px`
      box.style.height = `${Math.round(h)}px`
      box.style.left = `${Math.round(video.offsetLeft + (video.clientWidth - w) / 2)}px`
      box.style.top = `${Math.round(video.offsetTop + (video.clientHeight - h) / 2)}px`
    }

    const draw = () => {
      if (dead) return
      const now = video.currentTime * 1000 + p.offset.value
      let at = -1
      for (let i = sets.length - 1; i >= 0; i--) {
        if (sets[i]!.s <= now) {
          at = i
          break
        }
      }
      if (at !== drawn) {
        drawn = at
        const set = at >= 0 ? sets[at] : null
        // The canvas backs the FRAME, and composition space maps onto it
        // UNIFORMLY by width — the mirror of the burn path's compose(). A
        // 3840×1600 scope film carries subs authored against 1920×1080, and
        // stretching the composition canvas to the content box scales the axes
        // independently: text 1.35× too wide against burn-in and mpv.
        const vw = video.videoWidth || 1920
        const vh = video.videoHeight || 1080
        box.width = vw
        box.height = vh
        const ctx = box.getContext('2d')
        ctx?.clearRect(0, 0, vw, vh)
        if (set && ctx) {
          const scale = set.cw > 0 ? vw / set.cw : 1
          for (const object of set.objects) {
            const w = Math.max(1, Math.round(object.img.width * scale))
            const h = Math.max(1, Math.round(object.img.height * scale))
            // Bottom-anchored subs on a canvas taller than the picture clamp
            // back on screen, like burn-in.
            const x = Math.max(0, Math.min(Math.round(object.x * scale), vw - w))
            const y = Math.max(0, Math.min(Math.round(object.y * scale), vh - h))
            ctx.drawImage(object.img, x, y, w, h)
          }
        }
      }
      place()
    }
    const timer = window.setInterval(draw, DRAW_MS)
    video.addEventListener('seeked', draw)

    void (async () => {
      const response = await fetch(
        overlayUrl(selected, p.itemId.value, p.session.value.stream_url),
        { signal: aborter.signal },
      )
      // Reported, because `delivery` is `overlay` here and the `.vtt` <track>
      // renders only for `text`: there is nothing to fall back to. An image
      // track that fetches nothing is a picture with no subtitles on it, and no
      // way to tell that from a track that was always empty.
      if (!response.ok || !response.body) {
        if (!dead) playerNote('Subtitles for this track could not be loaded.')
        return
      }
      const reader = response.body.getReader()
      const decoder = new TextDecoder()
      let buffer = ''
      for (;;) {
        const { done, value } = await reader.read()
        if (dead) return
        if (value) buffer += decoder.decode(value, { stream: true })
        const cut = buffer.lastIndexOf('\n')
        if (cut >= 0) {
          for (const line of buffer.slice(0, cut).split('\n')) {
            if (!line.trim()) continue
            try {
              const set = JSON.parse(line) as {
                s: number
                cw: number
                ch: number
                o: { x: number; y: number; png: string }[]
              }
              const objects = await Promise.all(
                set.o.map(async (object) => {
                  const bytes = Uint8Array.from(atob(object.png), (ch) => ch.charCodeAt(0))
                  const img = await createImageBitmap(new Blob([bytes], { type: 'image/png' }))
                  return { x: object.x, y: object.y, img }
                }),
              )
              sets.push({ s: set.s, cw: set.cw, ch: set.ch, objects })
              drawn = -2 // force the next tick to re-decide
            } catch {
              // A partial line.
            }
          }
          buffer = buffer.slice(cut + 1)
        }
        if (done) break
      }
    })().catch(() => {
      // Aborts are ordinary and set `dead` first; anything else lost the track.
      if (!dead) playerNote('Subtitles for this track could not be loaded.')
    })

    imageDead = () => {
      dead = true
      aborter.abort()
      window.clearInterval(timer)
      video.removeEventListener('seeked', draw)
      box.style.display = 'none'
      box.getContext('2d')?.clearRect(0, 0, box.width, box.height)
    }
  }

  /// Faithful ASS rendering (HUB-32): JASSUB draws with libass on a canvas over
  /// the video, fed the original script and the source's embedded fonts.
  ///
  /// The .ass endpoint STREAMS on first extraction — header, then Dialogue
  /// lines as the demux pass reaches them — so the instance is created as soon
  /// as the header is in and later lines feed libass incrementally, rather than
  /// waiting out a full-file read.
  let assDead: (() => void) | null = null

  function assSubs() {
    const video = p.video.value
    const selected = p.selected.value
    if (!video || p.route.value !== 'ass' || !selected) return
    let dead = false
    let instance: JASSUB | null = null
    const aborter = new AbortController()

    void (async () => {
      let fonts: string[] = []
      try {
        const answer = await itemFonts(p.itemId.value)
        fonts = answer.fonts.map((_, at) => fontUrl(p.itemId.value, at))
      } catch {
        // No fonts: libass falls back.
      }
      if (dead) return
      // UX-4: libass is a worker plus a couple of megabytes of wasm, and only a
      // styled track ever needs it. Fetched inside the path that has already
      // decided this track is ASS — not at import time, where it rode along
      // with every session including audio-only ones.
      const { default: Jassub } = await loadChunk('jassub', () => import('jassub'))
      if (dead) return

      /// Feed JASSUB from a (possibly growing) ASS stream. True once the header
      /// was seen — that is, once the source really had the track.
      const feed = async (url: string): Promise<boolean> => {
        const response = await fetch(url, { signal: aborter.signal })
        if (!response.ok || !response.body) return false
        const reader = response.body.getReader()
        const decoder = new TextDecoder()
        let buffer = ''
        for (;;) {
          const { done, value } = await reader.read()
          if (dead) return true
          if (value) buffer += decoder.decode(value, { stream: true })
          if (!instance) {
            // Wait for the complete header: everything up to and including the
            // [Events] Format line.
            const events = buffer.toLowerCase().indexOf('[events]')
            const format = events >= 0 ? buffer.indexOf('Format:', events) : -1
            const end = format >= 0 ? buffer.indexOf('\n', format) : -1
            if (end >= 0) {
              instance = new Jassub({
                video,
                subContent: buffer.slice(0, end + 1),
                fonts,
                timeOffset: p.offset.value / 1000,
              })
              jassub = instance
              buffer = buffer.slice(end + 1)
            }
          }
          if (instance && buffer) {
            const cut = buffer.lastIndexOf('\n')
            if (cut >= 0) {
              const lines = buffer.slice(0, cut + 1)
              buffer = buffer.slice(cut + 1)
              await instance.ready
              if (dead) return true
              // The section header keeps libass's line parser in [Events]
              // whatever state it was left in by the initial track load.
              void (
                instance as unknown as { renderer: { processData: (s: string) => void } }
              ).renderer.processData(`[Events]\n${lines}`)
            }
          }
          if (done) return instance != null
        }
      }

      // The remux session taps embedded ASS live from the playback origin —
      // subtitles for what you are watching, instantly, at any position.
      // Sidecars and non-HLS fall back to the item endpoint, which is a
      // whole-file extraction, streamed.
      let fed = false
      if (p.isHls.value && selected.origin === 'embedded') {
        fed = await feed(
          sessionFileUrl(p.session.value.stream_url, `subs-${selected.id}.ass`),
        ).catch(() => false)
      }
      // These are different services — the tap comes off the mediahost through
      // the session, the fallback off the hub — so a tap that dies after its
      // header leaves the fallback perfectly able to answer, and it is the only
      // thing that can complete the script.
      //
      // Known, and deliberately not gated on `instance`: when the tap died
      // AFTER building the renderer, this appends the whole file on top of the
      // cues libass already holds, so the overlap is drawn twice. Gating on
      // `instance` was tried and is worse — it leaves a styled canvas with the
      // handful of cues that arrived and a complete copy sitting on the hub,
      // silently. Doubled text beats missing text.
      if (!fed && !dead) {
        await feed(subtitleFileUrl(p.itemId.value, `${selected.id}.ass`)).catch(() => false)
      }
      // On the INSTANCE, not on `fed`: a stream that dies after its header has
      // already put subtitles on screen and still returns false, so reporting
      // on `fed` would deny what the viewer can see. And there is no fallback
      // on this path — the `.vtt` <track> renders only for `delivery === 'text'`
      // — so a feed that never saw a header is simply no subtitles.
      if (!instance && !dead) playerNote('Subtitles for this track could not be loaded.')
    })().catch(() => {
      // In practice this is the `import('jassub')` and nothing else: both feeds
      // swallow their own rejections. `loadChunk` turns a missing chunk into one
      // reload, so what reaches here is a genuine failure.
      if (!dead) playerNote('Subtitles for this track could not be loaded.')
    })

    assDead = () => {
      dead = true
      aborter.abort()
      instance?.destroy()
      if (jassub === instance) jassub = null
    }
  }

  /// All three rebuild on the same inputs: which track, and where the run
  /// begins. `flush: 'post'` because two of them measure the element.
  watch(
    [p.route, p.subKey, p.epoch, p.itemId],
    () => {
      liveDead?.()
      liveDead = null
      imageDead?.()
      imageDead = null
      assDead?.()
      assDead = null
      liveText()
      imageSubs()
      assSubs()
    },
    { immediate: true, flush: 'post' },
  )

  onScopeDispose(() => {
    liveDead?.()
    imageDead?.()
    assDead?.()
    canvas?.remove()
    canvas = null
  })

  return {
    /// A seek-restart moves where the run begins, and the epoch bump rebuilds
    /// the renderers — but not before the next frame, and the skew is visible
    /// until it does. libass can be told directly.
    nudgeOffset(absMs: number) {
      if (jassub) jassub.timeOffset = absMs / 1000
    },
  }
}
