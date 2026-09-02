/// Where a poster comes from.
///
/// In `api/` rather than `domain/` because it builds a URL against the
/// generated bindings; the decisions ABOUT artwork — whose poster an episode
/// shows, and which version pins it — are in the components that read them.

import { getArtistArtworkUrl, getItemArtworkUrl } from './generated/kahawai.ts'

export type ArtSize = 'thumb' | 'card1x' | 'card'

export function artworkUrl(id: string, version?: number | null, size?: ArtSize): string {
  // Spread rather than assigned: with `exactOptionalPropertyTypes`, an
  // explicit `undefined` is not the same as an absent key, and the generated
  // params type says the key may be absent — not that it may be undefined.
  return getItemArtworkUrl(id, {
    ...(size ? { size } : {}),
    ...(version ? { v: String(version) } : {}),
  })
}

/// One poster at both densities, for the `srcset` of anything that shows a
/// card. What varies between clients here is the display, not the layout — the
/// widths are fixed — so these are `x` descriptors and there is no `sizes` to
/// get wrong. A 1× display stops being sent 6× the pixels it can show; a 2×
/// one is unaffected.
export function artworkSrcSet(id: string, version?: number | null): string {
  return `${artworkUrl(id, version, 'card1x')} 1x, ${artworkUrl(id, version, 'card')} 2x`
}

export function artistArtworkUrl(
  key: string,
  library: string,
  version?: number | null,
  size?: ArtSize,
): string {
  return getArtistArtworkUrl(key, {
    library,
    ...(size ? { size } : {}),
    ...(version ? { v: String(version) } : {}),
  })
}

export function artistArtworkSrcSet(key: string, library: string, version?: number | null): string {
  return `${artistArtworkUrl(key, library, version, 'card1x')} 1x, ${artistArtworkUrl(key, library, version, 'card')} 2x`
}
