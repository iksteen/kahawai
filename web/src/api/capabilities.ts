/// HUB-14: what this client can play, probed from the browser at runtime —
/// never a static claim. Sent with every session start and with the item
/// query; the hub decides direct/remux/transcode per stream from it.
///
/// The browser half is here; the decisions that do not need a browser are in
/// `domain/capability-mask.ts`, which is where they can be checked.

import type { CapabilityProfile } from './generated/model/capabilityProfile.ts'
import type { VideoCap } from './generated/model/videoCap.ts'
import {
  type AnnouncedVideo,
  applyMask,
  type CapabilityMask,
  maskSummary,
  rfc6381,
} from '../domain/capability-mask.ts'
import { pipMask, pipPhase } from '../domain/pip.ts'

/// One representative codec string per family, at generous profile and level
/// — the hub's verifier judges the SOURCE's profile against what is reported,
/// so probing the high end is what admits the most copies.
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
  // MSE speaks for the hls.js path; iOS Safari has no MediaSource, so fall
  // back to the video element's own claim, which is native HLS there.
  //
  // The METHOD is checked, not only the object. Calling it unconditionally
  // throws on a browser that has one without the other — inside the query the
  // item page runs, so every item page becomes "Could not load this item".
  if (typeof MediaSource !== 'undefined' && MediaSource?.isTypeSupported) {
    if (MediaSource.isTypeSupported(mime)) return true
  }
  if (typeof document === 'undefined') return false
  return document.createElement('video').canPlayType(mime) !== ''
}

const MASK_KEY = 'kahawai.capmask'

export function loadMask(): CapabilityMask {
  try {
    const raw = localStorage.getItem(MASK_KEY)
    return raw ? (JSON.parse(raw) as CapabilityMask) : {}
  } catch {
    // Unparseable, or storage denied: no mask.
    return {}
  }
}

export function saveMask(mask: CapabilityMask) {
  try {
    if (maskSummary(mask).length === 0) localStorage.removeItem(MASK_KEY)
    else localStorage.setItem(MASK_KEY, JSON.stringify(mask))
  } catch {
    // Private mode: the mask applies for this page's lifetime only.
  }
}

/// Probed once per page load; the answer only changes with the browser.
let cached: CapabilityProfile | null = null

/// The browser's own answer, mask NOT applied — what the debug panel offers to
/// subtract from, and the baseline it compares against.
export function probedProfile(): CapabilityProfile {
  if (cached) return cached
  cached = {
    containers: CONTAINER_PROBES.filter(([, mime]) => supported(mime)).map(([name]) => name),
    video: VIDEO_PROBES.filter(([, mime]) => supported(mime)).map(([codec]) => ({ codec })),
    audio: AUDIO_PROBES.filter(([, mime]) => supported(mime)).map(([name]) => name),
    // Browsers downmix natively; a ceiling would force re-encodes.
    max_audio_channels: 0,
    // "hdr" means this browser will DISPLAY HDR acceptably. Chrome and Safari
    // tone-map PQ in their compositor even on an SDR display; Firefox decodes
    // HEVC but renders PQ untouched — washed out — so it has to ask the server
    // to tone-map (HUB-15a). No feature probe exposes "I tone-map": this is
    // genuinely behavioural.
    hdr: typeof navigator !== 'undefined' && !navigator.userAgent.includes('Firefox'),
    ass_render: true, // the ASS renderer is bundled
    graphics_overlay: true, // the display-set canvas is bundled
    // Every browser renders WebVTT, and the player has no other text path: a
    // <track> takes VTT and nothing else. So this probes to a constant — its
    // whole value is being maskable, which is the only way to reach the
    // burn-in fallback for SRT and OCR tracks.
    vtt_render: true,
    // hls.js does not enforce the bound, and a browser reloading the playlist
    // every 2 s is why startup is quick. Declaring `accurate` would hand a
    // 10 s-GOP file a 12 s target and triple the runway the hub must build
    // before handover — for a player that never checks. The mask can force
    // either of the other two.
    target_duration: { mode: 'ignore' },
  }
  // A browser with no probeable video at all should not happen, and must not
  // send an empty list — that would transcode everything. A MASK emptying the
  // list is meaningful, and is applied after this.
  if (!cached.video?.length) cached.video = [{ codec: 'h264' }]
  if (!cached.audio?.length) cached.audio = ['aac', 'mp3']
  if (!cached.containers?.length) cached.containers = ['mp4']
  return cached
}

/// Precise caps for every announced video stream this browser verified.
function refineForSources(streams: AnnouncedVideo[]): VideoCap[] {
  const out: VideoCap[] = []
  const seen = new Set<string>()
  for (const video of streams) {
    const key = `${video.codec}/${video.profile}/${video.level}`
    if (seen.has(key)) continue
    seen.add(key)
    const mime = rfc6381(video)
    if (mime && supported(mime)) {
      out.push({ codec: video.codec, max_profile: video.profile!, max_level: video.level! })
    }
  }
  return out
}

export function buildProfile(
  bandwidthKbps?: number | null,
  announced?: AnnouncedVideo[],
): CapabilityProfile {
  const base = probedProfile()
  const profile: CapabilityProfile = { ...base, video: [...(base.video ?? [])] }
  if (bandwidthKbps && bandwidthKbps > 0) profile.max_bandwidth_kbps = bandwidthKbps
  // Exact per-stream verifications ride ALONGSIDE the family floor — the hub
  // admits a stream when any cap for its codec does.
  if (announced?.length) profile.video?.push(...refineForSources(announced))
  // The mask goes LAST: a source-aware precise cap must not smuggle back a
  // family the mask has just dropped.
  const masked = applyMask(profile, loadMask())
  // PiP renders in a window the overlay canvases cannot follow, so every
  // session started while PiP is intended — the deliberate restart AND any
  // recovery while the window is up — negotiates as a client without them.
  if (pipPhase.value === 'off') return masked
  return applyMask(masked, pipMask())
}

/// For tests, which need a browser that answers differently from the one they
/// are running in.
export function forgetProbe() {
  cached = null
}
