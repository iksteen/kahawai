/// Which subtitle track to show, and where its pixels come from.
///
/// The hub computes `delivery` per track FOR THE ASKING CLIENT (HUB-32a/b/c),
/// so everything here is about a listing that already knows what this browser
/// can be served — a masked run changes these answers, not just the wording.

import type { TrackListing } from '../api/generated/model/trackListing.ts'

/// A bitmap track: its cues are pictures, not text.
export const isImageSub = (track: Pick<TrackListing, 'format'>) =>
  ['pgs', 'vobsub', 'dvdsub'].includes(track.format)

/// HUB-32d: a styled script rendered server-side to display sets. Delivered as
/// an overlay like PGS, but sourced item-level rather than from the live
/// session tap.
export const isRasterSub = (track: Pick<TrackListing, 'origin'>) => track.origin === 'raster'

/// The first track a language wishlist would choose, or nothing.
///
/// Language wishes auto-pick only CLIENT-RENDERED tracks: silently forcing a
/// burn is a video encode restart, which is never what a language preference
/// means. Burns are explicit picks.
export function pickSubtitle<T extends Pick<TrackListing, 'delivery' | 'format' | 'language'>>(
  wishlist: string[],
  subs: T[],
): T | null {
  const auto = (s: T) => s.delivery === 'text' || s.delivery === 'ass' || s.delivery === 'overlay'
  // The server's fidelity order (HUB-32a/d): the client's own ASS renderer
  // first, then a server-rasterised overlay, then flattened text. Within one
  // language the BEST reading wins, not whichever row the listing happened to
  // put first — otherwise a client with ASS masked off would take the flattened
  // VTT and never notice the rasterised track sitting right behind it.
  const rank = (s: T) => (s.delivery === 'ass' ? 0 : s.delivery === 'overlay' ? 1 : 2)
  const best = (candidates: T[]) =>
    candidates.length === 0 ? null : candidates.reduce((a, b) => (rank(b) < rank(a) ? b : a))
  for (const want of wishlist) {
    const eligible = subs.filter((s) => auto(s) && !isImageSub(s))
    const hit =
      want === 'any'
        ? best(eligible)
        : best(
            eligible.filter(
              (s) => (s.language ?? '').toLowerCase().slice(0, 2) === want.slice(0, 2),
            ),
          )
    if (hit) return hit
  }
  return null
}
