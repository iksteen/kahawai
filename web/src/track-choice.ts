import { pickSubtitle, type StreamVerdict, type Subtitle } from './api.ts'

/// Which subtitle track a session opens with.
///
/// Exact memory first — the id is the only spelling that can name a downloaded
/// or OCR row, which no language wish will ever match. A remembered track the
/// hub now says it cannot serve (`delivery: 'none'`) is not a choice, so the
/// wishlist gets its turn instead of the viewer getting nothing.
export function initialSubtitle(p: {
  subs: Subtitle[]
  exactId: number | null
  wishlist: string[]
}): Subtitle | null {
  const exact = p.subs.find((s) => s.id === p.exactId && s.delivery !== 'none')
  return exact ?? pickSubtitle(p.wishlist, p.subs)
}

/// Whether re-applying a remembered burn means restarting the pipeline.
///
/// A burn lives server-side, so choosing one is an encode. If this session is
/// already burning that very track there is nothing to do, and doing it anyway
/// spends a restart on the picture the viewer is already watching.
export function needsBurnRestart(pick: Subtitle, streams: StreamVerdict | undefined): boolean {
  if (pick.delivery !== 'burn') return false
  return !streams?.subtitles?.some((v) => v.track_id === pick.id && v.tier === 'burn')
}
