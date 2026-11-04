/// Where a poster comes from.
///
/// In `api/` rather than `domain/` because it builds a URL against the
/// generated bindings; the decisions ABOUT artwork — whose poster an episode
/// shows, and which version pins it — are in the components that read them.

import {
  getCatalogueArtistArtworkUrl,
  getCatalogueArtworkUrl,
  getCatalogueCastArtworkUrl,
} from './generated/kahawai.ts'
import type { ArtworkKind } from './generated/model/artworkKind.ts'

export type ArtSize = 'thumb' | 'card1x' | 'card'
export type ArtSelection = { kind?: ArtworkKind; season?: number | null; image?: string | null }

// A Vue key cannot invalidate the browser's decoded-image cache. Put a stable
// revision in the URL itself, changing only when the selected image changes.
// FNV-1a keeps provider URLs and local artwork references out of the query.
function withImageRevision(url: string, image?: string | null): string {
  if (!url || !image) return url
  let revision = 0xcbf29ce484222325n
  for (const byte of new TextEncoder().encode(image)) {
    revision = BigInt.asUintN(64, (revision ^ BigInt(byte)) * 0x100000001b3n)
  }
  const result = new URL(url, 'http://kahawai.invalid')
  result.searchParams.set('v', revision.toString(16).padStart(16, '0'))
  return `${result.pathname}${result.search}`
}

export function artworkUrl(
  id: string,
  _version?: number | null,
  size?: ArtSize,
  library?: string | null,
  selection: ArtSelection = {},
): string {
  // Spread rather than assigned: with `exactOptionalPropertyTypes`, an
  // explicit `undefined` is not the same as an absent key, and the generated
  // params type says the key may be absent — not that it may be undefined.
  return library
    ? withImageRevision(
        getCatalogueArtworkUrl(encodeURIComponent(library), encodeURIComponent(id), {
          ...(size ? { size } : {}),
          ...(selection.kind ? { kind: selection.kind } : {}),
          ...(selection.season !== undefined
            ? { season: selection.season === null ? 'absolute' : String(selection.season) }
            : {}),
        }),
        selection.image,
      )
    : ''
}

/// One poster at both densities, for the `srcset` of anything that shows a
/// card. What varies between clients here is the display, not the layout — the
/// widths are fixed — so these are `x` descriptors and there is no `sizes` to
/// get wrong. A 1× display stops being sent 6× the pixels it can show; a 2×
/// one is unaffected.
export function artworkSrcSet(
  id: string,
  version?: number | null,
  library?: string | null,
  selection: ArtSelection = {},
): string {
  return `${artworkUrl(id, version, 'card1x', library, selection)} 1x, ${artworkUrl(id, version, 'card', library, selection)} 2x`
}

export function castArtworkUrl(
  id: string,
  person: string,
  library: string,
  size: ArtSize,
  image?: string | null,
): string {
  return withImageRevision(
    getCatalogueCastArtworkUrl(
      encodeURIComponent(library),
      encodeURIComponent(id),
      encodeURIComponent(person),
      { size },
    ),
    image,
  )
}

export function artistArtworkUrl(key: string, library: string, size?: ArtSize): string {
  const params = new URLSearchParams({
    ...(size ? { size } : {}),
  })
  return `${getCatalogueArtistArtworkUrl(encodeURIComponent(library), encodeURIComponent(key))}${params.size ? `?${params}` : ''}`
}

export function artistArtworkSrcSet(key: string, library: string): string {
  return `${artistArtworkUrl(key, library, 'card1x')} 1x, ${artistArtworkUrl(key, library, 'card')} 2x`
}
