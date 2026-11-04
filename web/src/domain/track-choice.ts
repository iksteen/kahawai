import type { PlaybackStreams } from '../api/generated/model/playbackStreams.ts'
import { pickSubtitle } from './subtitles.ts'
import type { SubtitleKind } from './prefs.ts'
import type { TrackListing } from '../api/generated/model/trackListing.ts'

/// Which subtitle track a session opens with.
///
/// Exact memory first — the id is the only spelling that can name a downloaded
/// or OCR row, which no language wish will ever match. A remembered track the
/// hub now says it cannot serve (`delivery: 'none'`) leaves the choice to the
/// language wishlist and its ordered subtitle-type preferences.
export function initialSubtitle(p: {
  subs: TrackListing[]
  exactId: number | null
  wishlist: string[]
  kindOrder?: readonly SubtitleKind[]
}): TrackListing | null {
  const exact = p.subs.find((s) => s.id === p.exactId && s.delivery !== 'none')
  return exact ?? pickSubtitle(p.wishlist, p.subs, p.kindOrder)
}

/// Whether re-applying a remembered burn means restarting the pipeline.
///
/// A burn lives server-side, so choosing one is an encode. If this session is
/// already burning that very track there is nothing to do, and doing it anyway
/// spends a restart on the picture the viewer is already watching.
export function needsBurnRestart(
  pick: TrackListing,
  streams: PlaybackStreams | null | undefined,
): boolean {
  if (pick.delivery !== 'burn') return false
  return !streams?.subtitles?.some((v) => v.track_id === pick.id && v.tier === 'burn')
}
