/// The debug capability mask: what a client is allowed to pretend it cannot
/// do.
///
/// The negotiation matrix (HUB-14/15) and the subtitle tiers (HUB-32a/b) have
/// branches that most browsers never take — a client that cannot decode HEVC,
/// cannot display HDR, cannot render ASS, cannot composite bitmap display
/// sets. Hunting for a browser that genuinely lacks each one is slow and
/// unrepeatable, so the client can SUBTRACT from its own probe. What the mask
/// changes is not cosmetic: the profile sent to the hub and the rendering the
/// player performs both follow the masked answer, so a masked client behaves
/// like the real thing.
///
/// Codecs and containers can only be DROPPED — claiming a decoder the browser
/// lacks produces a stream it cannot play, which tests nothing. The three
/// booleans are declarations rather than probes (the ASS renderer and the
/// display-set canvas are always bundled; `hdr` is a behavioural claim no
/// feature test exposes), so those may go either way.

import type { CapabilityProfile } from '../api/generated/model/capabilityProfile.ts'
import type { TargetDuration } from '../api/generated/model/targetDuration.ts'

export type CapabilityMask = {
  /// Codec families and containers to drop from the probed lists.
  video?: string[]
  audio?: string[]
  containers?: string[]
  /// Ceilings to impose; absent means whatever the probe allowed.
  max_bit_depth?: number
  max_height?: number
  max_audio_channels?: number
  /// Declaration overrides; absent means the probe's own answer.
  hdr?: boolean
  ass_render?: boolean
  graphics_overlay?: boolean
  vtt_render?: boolean
  target_duration?: TargetDuration
}

/// What this mask actually changes, one token each; `[]` means inert.
///
/// Shown wherever a mask can be set, so a mask left on can never be mistaken
/// for a bug — which is the whole trap this affordance would otherwise set.
export function maskSummary(mask: CapabilityMask): string[] {
  const out: string[] = []
  for (const kind of ['video', 'audio', 'containers'] as const) {
    const dropped = mask[kind]
    if (dropped?.length) out.push(`−${dropped.join(',')}`)
  }
  if (mask.max_bit_depth) out.push(`≤${mask.max_bit_depth}-bit`)
  if (mask.max_height) out.push(`≤${mask.max_height}p`)
  if (mask.max_audio_channels) out.push(`${mask.max_audio_channels}ch`)
  for (const flag of ['hdr', 'ass_render', 'graphics_overlay', 'vtt_render'] as const) {
    if (mask[flag] !== undefined) out.push(`${flag}=${mask[flag]}`)
  }
  if (mask.target_duration) {
    const target = mask.target_duration
    out.push(`target=${target.mode === 'short' ? `short:${target.max_secs}s` : target.mode}`)
  }
  return out
}

export function applyMask(profile: CapabilityProfile, mask: CapabilityMask): CapabilityProfile {
  const out: CapabilityProfile = { ...profile, video: [...(profile.video ?? [])] }
  // Each list is filtered only when the mask names something to drop, and only
  // when the profile has one — an absent list and an empty one are different
  // claims, and this must not turn the first into the second.
  const drop = mask.video
  if (drop?.length && out.video) out.video = out.video.filter((c) => !drop.includes(c.codec))
  if (mask.audio?.length && out.audio) {
    const dropped = mask.audio
    out.audio = out.audio.filter((c) => !dropped.includes(c))
  }
  if (mask.containers?.length && out.containers) {
    const dropped = mask.containers
    out.containers = out.containers.filter((c) => !dropped.includes(c))
  }
  // Ceilings tighten, never loosen, including every source-aware entry.
  if (out.video && mask.max_bit_depth && [8, 10, 12, 16].includes(mask.max_bit_depth)) {
    out.video = out.video.map((cap) => ({
      ...cap,
      max_bit_depth: Math.min(cap.max_bit_depth ?? mask.max_bit_depth!, mask.max_bit_depth!),
    }))
  }
  // Ceilings tighten, never loosen.
  if (mask.max_height) out.max_height = Math.min(mask.max_height, out.max_height ?? mask.max_height)
  if (mask.max_audio_channels) out.max_audio_channels = mask.max_audio_channels
  if (mask.hdr !== undefined) out.hdr = mask.hdr
  if (mask.ass_render !== undefined) out.ass_render = mask.ass_render
  if (mask.graphics_overlay !== undefined) out.graphics_overlay = mask.graphics_overlay
  if (mask.vtt_render !== undefined) out.vtt_render = mask.vtt_render
  if (mask.target_duration !== undefined) out.target_duration = mask.target_duration
  return out
}

/// GStreamer caps profile → avc1 profile_idc + constraint bytes.
const H264_PROFILE_HEX: Record<string, string> = {
  'constrained-baseline': '42E0',
  baseline: '4200',
  main: '4D40',
  high: '6400',
  'high-10': '6E00',
}

/// "4.1" → "29" (hex of 41); undefined on junk.
function h264LevelHex(level: string): string | undefined {
  const [major, minor = '0'] = level.split('.')
  const n = Number(major) * 10 + Number(minor)
  return Number.isFinite(n) && n > 0 ? n.toString(16).toUpperCase().padStart(2, '0') : undefined
}

export type AnnouncedVideo = {
  codec: string
  profile?: string | null
  level?: string | null
  bit_depth?: number | null
}

// Old hubs may still expose discoverer's aggregate depths during rollout.
export function probeDepth(video: AnnouncedVideo): number {
  const depth = video.bit_depth === 24 ? 8 : video.bit_depth === 30 ? 10 : video.bit_depth
  if (depth && [8, 10, 12, 16].includes(depth)) return depth
  if (video.profile === 'main-10' || video.profile === 'high-10') return 10
  return 8
}

/// The exact codec string for an announced stream, or undefined when the
/// metadata predates the probe extension — in which case the generic family
/// floor applies and nothing precise is claimed.
///
/// The generic matrix answers "does this browser know the codec FAMILY"; this
/// lets the client ask the exact question instead, and a passing probe becomes
/// a precise cap that composes with the floor.
export function rfc6381(video: AnnouncedVideo): string | undefined {
  if (!video.profile || !video.level) return undefined
  if (video.codec === 'h264') {
    if (probeDepth(video) > (video.profile === 'high-10' ? 10 : 8)) return undefined
    const profile = H264_PROFILE_HEX[video.profile]
    const level = h264LevelHex(video.level)
    return profile && level ? `video/mp4; codecs="avc1.${profile}${level}"` : undefined
  }
  // Codec strings carry component depth explicitly for VP9 and AV1.
  // https://www.webmproject.org/vp9/mp4/#codecs-parameter-string
  // https://aomediacodec.github.io/av1-isobmff/#codecsparam
  if (video.codec === 'vp9') {
    const profile = Number(video.profile)
    const level = Math.round(Number(video.level) * 10)
    const depth = probeDepth(video)
    if (
      !Number.isInteger(profile) ||
      profile < 0 ||
      profile > 3 ||
      !Number.isFinite(level) ||
      level <= 0
    )
      return undefined
    if ((profile < 2 && depth !== 8) || (profile >= 2 && ![10, 12].includes(depth)))
      return undefined
    return `video/webm; codecs="vp09.${String(profile).padStart(2, '0')}.${String(level).padStart(2, '0')}.${String(depth).padStart(2, '0')}"`
  }
  if (video.codec === 'av1') {
    const profile = { main: 0, high: 1, professional: 2 }[video.profile]
    const level = Number(video.level)
    const major = Math.floor(level),
      minor = Math.round((level - major) * 10)
    const index = (major - 2) * 4 + minor
    const depth = probeDepth(video)
    if (
      profile === undefined ||
      !Number.isFinite(index) ||
      major < 2 ||
      minor > 3 ||
      index > 23 ||
      ![8, 10, 12].includes(depth) ||
      (depth === 12 && profile !== 2)
    )
      return undefined
    return `video/mp4; codecs="av01.${profile}.${String(index).padStart(2, '0')}M.${String(depth).padStart(2, '0')}"`
  }
  if (video.codec === 'hevc') {
    if (probeDepth(video) > (video.profile === 'main-10' ? 10 : 8)) return undefined
    const [major, minor = '0'] = video.level.split('.')
    const n = (Number(major) * 10 + Number(minor)) * 3
    if (!Number.isFinite(n) || n <= 0) return undefined
    if (video.profile === 'main') return `video/mp4; codecs="hvc1.1.6.L${n}.B0"`
    if (video.profile === 'main-10') return `video/mp4; codecs="hvc1.2.4.L${n}.B0"`
  }
  return undefined
}
