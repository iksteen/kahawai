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

/** Probe once per page load; the result only changes with the browser. */
let cached: CapabilityProfile | null = null

export function buildProfile(bandwidthKbps?: number | null): CapabilityProfile {
  if (!cached) {
    cached = {
      containers: CONTAINER_PROBES.filter(([, m]) => supported(m)).map(([n]) => n),
      video: VIDEO_PROBES.filter(([, m]) => supported(m)).map(([codec]) => ({ codec })),
      audio: AUDIO_PROBES.filter(([, m]) => supported(m)).map(([n]) => n),
      // Browsers downmix natively; a ceiling would force re-encodes.
      max_audio_channels: 0,
      hdr: window.matchMedia?.('(dynamic-range: high)')?.matches ?? false,
      ass_render: true, // JASSUB is bundled
      graphics_overlay: true, // canvas display-set renderer is bundled
    }
    // A browser with no probeable video at all (should not happen)
    // must not send an empty list — that would transcode everything.
    if (cached.video.length === 0) cached.video = [{ codec: 'h264' }]
    if (cached.audio.length === 0) cached.audio = ['aac', 'mp3']
    if (cached.containers.length === 0) cached.containers = ['mp4']
  }
  const p = { ...cached }
  if (bandwidthKbps && bandwidthKbps > 0) p.max_bandwidth_kbps = bandwidthKbps
  return p
}
