import type { Subtitle } from './api'

/// Which of the player's five subtitle paths a chosen track takes.
///
/// `burned` and `none` render nothing client-side and are not the same thing:
/// one is already in the picture, the other is a track this client cannot be
/// served.
export type SubtitleRoute = 'none' | 'burned' | 'ass' | 'image' | 'live-text' | 'vtt-track'

/// The hub computed `delivery` from the bits this client declared, so a masked
/// run changes what happens here and not just what the verdict text says.
///
/// `live-text` keys on the FORMAT rather than on the ass route: the pipeline
/// taps an ASS track as .ass and never writes the .jsonl this path reads, so a
/// client that declined ASS must go to the flattened .vtt rather than chase a
/// tap that cannot exist.
export function subtitleRoute(
  selected: Pick<Subtitle, 'delivery' | 'format' | 'origin'> | undefined,
  ctx: { isHls: boolean; vttFallback: boolean },
): SubtitleRoute {
  if (!selected) return 'none'
  if (selected.delivery === 'none') return 'none'
  if (selected.delivery === 'burn') return 'burned'
  if (selected.delivery === 'ass') return 'ass'
  if (selected.delivery === 'overlay') return 'image'
  if (selected.delivery !== 'text') {
    // Only reachable if a sixth delivery is added to the union: render nothing
    // rather than fall through and request a .vtt for something that may not
    // have one, which is the load that hangs forever. The `never` makes it a
    // compile error rather than a discovery.
    const unhandled: never = selected.delivery
    void unhandled
    return 'none'
  }
  const isAss = selected.format === 'ass' || selected.format === 'ssa'
  // A <track> cannot consume a growing document, so an embedded text track on a
  // live playlist is fed cue by cue from the session tap instead — until the tap
  // yields nothing (old satellite, no pipeline), which is what the fallback is.
  const live = ctx.isHls && !isAss && selected.origin === 'embedded' && !ctx.vttFallback
  return live ? 'live-text' : 'vtt-track'
}
