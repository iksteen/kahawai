/// Picture-in-Picture as a NEGOTIATION, not an overlay swap.
///
/// The ASS canvas and the display-set canvas live next to the <video> in the
/// page; the PiP window takes only the element, so every client-composited
/// subtitle stays behind in the original tab. Re-parenting canvases into the
/// PiP window was tried and abandoned — the browser owns that window's
/// contents. What works is honesty: a client about to enter PiP CANNOT
/// composite overlays, so it renegotiates as one that lacks them (the hub
/// burns or falls back to VTT, which the browser renders inside PiP itself),
/// and renegotiates back on leave.
///
/// Module-level because the picture is remounted per session: the intent has
/// to survive the very restart it causes.
import { ref } from 'vue'
import type { CapabilityMask } from './capability-mask.ts'

export const pipSupported =
  typeof HTMLVideoElement !== 'undefined' && 'requestPictureInPicture' in HTMLVideoElement.prototype

/// off → entering (masked restart under way; enter PiP when the new picture
/// plays) → on (in the PiP window; leaving restarts unmasked).
export const pipPhase = ref<'off' | 'entering' | 'on'>('off')

/// What the masked restart declines. The element-PiP window takes only the
/// <video>, so the in-page canvases are lost — and whether the browser
/// paints native <track> cues inside its own window varies by engine and
/// has no feature probe. So the text path goes too, and the hub burns the
/// track in: the one form guaranteed visible in ANY PiP window. This path
/// is only a fallback — everything with Document PiP (Chromium, Firefox
/// 153+) takes the window with the canvases inside it and never masks.
export function pipMask(): CapabilityMask {
  return { ass_render: false, graphics_overlay: false, vtt_render: false }
}
