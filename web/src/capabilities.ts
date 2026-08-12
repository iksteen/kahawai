// HUB-14: the client's capability profile, probed from the browser at
// runtime — never a static claim. Sent with every session start; the
// hub decides direct/remux/transcode per stream from it.

export type VideoCap = { codec: string; max_profile?: string; max_level?: string }
export type CapabilityProfile = {
  containers: string[]
  video: VideoCap[]
  audio: string[]
  max_audio_channels: number
  max_height?: number
  max_fps?: number
  hdr: boolean
  max_bandwidth_kbps?: number
  ass_render: boolean
  graphics_overlay: boolean
  vtt_render: boolean
  target_duration: TargetDuration
}

/// What this client needs from EXT-X-TARGETDURATION. Required — the
/// server has no default, because the right answer differs per client
/// and guessing means either a spec violation or a startup delay.
export type TargetDuration =
  | { mode: 'ignore' }
  | { mode: 'accurate' }
  | { mode: 'short'; max_secs: number }

// One representative codec string per family, at generous profile/level
// — the hub's verifier judges the SOURCE's profile against what we
// report, so probing the high end is what admits the most copies.
const VIDEO_PROBES: [string, string][] = [
  ['h264', 'video/mp4; codecs="avc1.640033"'], // High L5.1
  ['hevc', 'video/mp4; codecs="hvc1.2.4.L153.B0"'], // Main 10
  ['vp9', 'video/webm; codecs="vp09.00.50.08"'],
  ['av1', 'video/mp4; codecs="av01.0.08M.08"'],
]
const AUDIO_PROBES: [string, string][] = [
  ['aac', 'audio/mp4; codecs="mp4a.40.2"'],
  ['mp3', 'audio/mpeg'],
  ['opus', 'audio/webm; codecs="opus"'],
  ['flac', 'audio/mp4; codecs="flac"'],
]
const CONTAINER_PROBES: [string, string][] = [
  ['mp4', 'video/mp4'],
  ['webm', 'video/webm'],
]

function supported(mime: string): boolean {
  // MSE speaks for the hls.js path; iOS Safari has no MediaSource, so
  // fall back to the <video> element's own claim (native HLS there).
  if (typeof MediaSource !== 'undefined' && MediaSource.isTypeSupported) {
    if (MediaSource.isTypeSupported(mime)) return true
  }
  const v = document.createElement('video')
  return v.canPlayType(mime) !== ''
}

// ---- Source-aware refinement (HUB-14 precision) ----
//
// The generic matrix answers "does this browser know the codec FAMILY";
// the announced source metadata lets us ask the exact question: build
// the RFC-6381 string for the stream's own profile/level and probe
// THAT. A passing probe becomes a precise VideoCap (the hub admits any
// matching cap, so precise entries compose with the family floor).

/// GStreamer caps profile → avc1 profile_idc+constraint bytes.
const H264_PROFILE_HEX: Record<string, string> = {
  'constrained-baseline': '42E0',
  baseline: '4200',
  main: '4D40',
  high: '6400',
  'high-10': '6E00',
}

/// "4.1" → "29" (hex of 41); undefined on junk.
function h264LevelHex(level: string): string | undefined {
  const [maj, min = '0'] = level.split('.')
  const n = Number(maj) * 10 + Number(min)
  return Number.isFinite(n) && n > 0 ? n.toString(16).toUpperCase().padStart(2, '0') : undefined
}

type AnnouncedVideo = { codec: string; profile?: string | null; level?: string | null }

/// The exact codec string for an announced stream, or undefined when
/// the metadata predates the probe extension (generic floor applies).
function rfc6381(v: AnnouncedVideo): string | undefined {
  if (!v.profile || !v.level) return undefined
  if (v.codec === 'h264') {
    const p = H264_PROFILE_HEX[v.profile]
    const l = h264LevelHex(v.level)
    return p && l ? `video/mp4; codecs="avc1.${p}${l}"` : undefined
  }
  if (v.codec === 'hevc') {
    const [maj, min = '0'] = v.level.split('.')
    const n = (Number(maj) * 10 + Number(min)) * 3
    if (!Number.isFinite(n) || n <= 0) return undefined
    if (v.profile === 'main') return `video/mp4; codecs="hvc1.1.6.L${n}.B0"`
    if (v.profile === 'main-10') return `video/mp4; codecs="hvc1.2.4.L${n}.B0"`
  }
  return undefined
}

/// Precise caps for every announced video stream this browser verified.
function refineForSources(streams: AnnouncedVideo[]): VideoCap[] {
  const out: VideoCap[] = []
  const seen = new Set<string>()
  for (const v of streams) {
    const key = `${v.codec}/${v.profile}/${v.level}`
    if (seen.has(key)) continue
    seen.add(key)
    const mime = rfc6381(v)
    if (mime && supported(mime)) {
      out.push({ codec: v.codec, max_profile: v.profile!, max_level: v.level! })
    }
  }
  return out
}

// ---- Debug capability mask ----
//
// The negotiation matrix (HUB-14/15) and the subtitle tiers (HUB-32a/b)
// have branches that most browsers never take: a client that cannot
// decode HEVC, cannot display HDR, cannot render ASS, cannot composite
// bitmap display sets. Hunting for a browser that genuinely lacks each
// one is slow and unrepeatable, so the player can SUBTRACT from the
// probe. What the mask changes is not cosmetic: the profile sent to the
// hub and the rendering the player itself performs both follow the
// masked answer, so a masked client behaves like the real thing.
//
// Codec/container entries can only be dropped — claiming a decoder the
// browser lacks would produce a stream it cannot play, which tests
// nothing. The three booleans are declarations rather than probes
// (JASSUB and the display-set canvas are always bundled; `hdr` is a
// behavioural claim no feature test exposes), so those may go either
// way.

const MASK_KEY = 'kahawai.capmask'

export type CapabilityMask = {
  /** Codec families / containers to drop from the probed lists. */
  video?: string[]
  audio?: string[]
  containers?: string[]
  /** Ceilings to impose; absent = whatever the probe allowed. */
  max_height?: number
  max_audio_channels?: number
  /** Declaration overrides; absent = the probe's own answer. */
  hdr?: boolean
  ass_render?: boolean
  graphics_overlay?: boolean
  vtt_render?: boolean
  target_duration?: TargetDuration
}

export function loadMask(): CapabilityMask {
  try {
    const raw = localStorage.getItem(MASK_KEY)
    return raw ? (JSON.parse(raw) as CapabilityMask) : {}
  } catch {
    return {} // unparseable or storage denied: no mask
  }
}

export function saveMask(m: CapabilityMask) {
  try {
    if (maskSummary(m).length === 0) localStorage.removeItem(MASK_KEY)
    else localStorage.setItem(MASK_KEY, JSON.stringify(m))
  } catch {
    // private mode: the mask applies for this page's lifetime only
  }
}

/**
 * What this mask actually changes, one token each; `[]` means inert.
 * The player shows these so a mask left on can never be mistaken for a
 * bug — the whole trap this affordance would otherwise set.
 */
export function maskSummary(m: CapabilityMask): string[] {
  const out: string[] = []
  for (const kind of ['video', 'audio', 'containers'] as const) {
    const dropped = m[kind]
    if (dropped?.length) out.push(`−${dropped.join(',')}`)
  }
  if (m.max_height) out.push(`≤${m.max_height}p`)
  if (m.max_audio_channels) out.push(`${m.max_audio_channels}ch`)
  for (const flag of ['hdr', 'ass_render', 'graphics_overlay', 'vtt_render'] as const) {
    if (m[flag] !== undefined) out.push(`${flag}=${m[flag]}`)
  }
  if (m.target_duration) {
    const t = m.target_duration
    out.push(`target=${t.mode === 'short' ? `short:${t.max_secs}s` : t.mode}`)
  }
  return out
}

function applyMask(p: CapabilityProfile, m: CapabilityMask): CapabilityProfile {
  const out: CapabilityProfile = { ...p, video: [...p.video] }
  if (m.video?.length) out.video = out.video.filter((c) => !m.video!.includes(c.codec))
  if (m.audio?.length) out.audio = out.audio.filter((c) => !m.audio!.includes(c))
  if (m.containers?.length)
    out.containers = out.containers.filter((c) => !m.containers!.includes(c))
  // Ceilings tighten, never loosen.
  if (m.max_height) out.max_height = Math.min(m.max_height, out.max_height ?? m.max_height)
  if (m.max_audio_channels) out.max_audio_channels = m.max_audio_channels
  if (m.hdr !== undefined) out.hdr = m.hdr
  if (m.ass_render !== undefined) out.ass_render = m.ass_render
  if (m.graphics_overlay !== undefined) out.graphics_overlay = m.graphics_overlay
  if (m.vtt_render !== undefined) out.vtt_render = m.vtt_render
  if (m.target_duration !== undefined) out.target_duration = m.target_duration
  return out
}

/** Probe once per page load; the result only changes with the browser. */
let cached: CapabilityProfile | null = null

/**
 * The browser's own answer, mask NOT applied — what the debug panel
 * offers to subtract from, and the baseline it compares against.
 */
export function probedProfile(): CapabilityProfile {
  if (!cached) {
    cached = {
      containers: CONTAINER_PROBES.filter(([, m]) => supported(m)).map(([n]) => n),
      video: VIDEO_PROBES.filter(([, m]) => supported(m)).map(([codec]) => ({ codec })),
      audio: AUDIO_PROBES.filter(([, m]) => supported(m)).map(([n]) => n),
      // Browsers downmix natively; a ceiling would force re-encodes.
      max_audio_channels: 0,
      // "hdr" = this browser will DISPLAY HDR acceptably. Chrome and
      // Safari tone-map PQ in their compositor even on SDR displays;
      // Firefox decodes HEVC but renders PQ untouched (washed out), so
      // it must ask the server to tone-map (HUB-15a). No feature probe
      // exposes "I tone-map" — this is genuinely behavioral.
      hdr: !navigator.userAgent.includes('Firefox'),
      ass_render: true, // JASSUB is bundled
      graphics_overlay: true, // canvas display-set renderer is bundled
      // Every browser renders WebVTT, and the player has no other text
      // path: a <track> takes VTT and nothing else, and the live cue
      // tap feeds the same TextTrack renderer. So this probes to a
      // constant — its whole value is being maskable, which is the only
      // way to reach the burn-in fallback for SRT and OCR tracks.
      vtt_render: true,
      // hls.js does not enforce the bound, and a browser reloading the
      // playlist every 2 s is why startup is quick. Declaring
      // `accurate` here would hand a 10 s-GOP file a 12 s target and
      // triple the runway the hub must build before handover — for a
      // player that never checks. The mask can force the other two.
      target_duration: { mode: 'ignore' },
    }
    // A browser with no probeable video at all (should not happen)
    // must not send an empty list — that would transcode everything.
    // (A mask emptying the list IS meaningful and is applied later.)
    if (cached.video.length === 0) cached.video = [{ codec: 'h264' }]
    if (cached.audio.length === 0) cached.audio = ['aac', 'mp3']
    if (cached.containers.length === 0) cached.containers = ['mp4']
  }
  return cached
}

export function buildProfile(
  bandwidthKbps?: number | null,
  announced?: AnnouncedVideo[],
): CapabilityProfile {
  const base = probedProfile()
  const p: CapabilityProfile = { ...base, video: [...base.video] }
  if (bandwidthKbps && bandwidthKbps > 0) p.max_bandwidth_kbps = bandwidthKbps
  // Exact per-stream verifications ride ALONGSIDE the family floor —
  // the hub admits a stream when any cap for its codec does.
  if (announced?.length) p.video.push(...refineForSources(announced))
  // The mask goes LAST: a source-aware precise cap must not smuggle
  // back a family the mask just dropped.
  return applyMask(p, loadMask())
}
