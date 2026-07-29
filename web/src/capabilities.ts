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
}

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

/** Probe once per page load; the result only changes with the browser. */
let cached: CapabilityProfile | null = null

export function buildProfile(
  bandwidthKbps?: number | null,
  announced?: AnnouncedVideo[],
): CapabilityProfile {
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
    }
    // A browser with no probeable video at all (should not happen)
    // must not send an empty list — that would transcode everything.
    if (cached.video.length === 0) cached.video = [{ codec: 'h264' }]
    if (cached.audio.length === 0) cached.audio = ['aac', 'mp3']
    if (cached.containers.length === 0) cached.containers = ['mp4']
  }
  const p = { ...cached, video: [...cached.video] }
  if (bandwidthKbps && bandwidthKbps > 0) p.max_bandwidth_kbps = bandwidthKbps
  // Exact per-stream verifications ride ALONGSIDE the family floor —
  // the hub admits a stream when any cap for its codec does.
  if (announced?.length) p.video.push(...refineForSources(announced))
  return p
}
