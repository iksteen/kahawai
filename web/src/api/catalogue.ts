/// Translate catalogue descriptions into the existing presentation models.
/// User state comes from the hub alongside the descriptions; playback is separate.
import {
  items,
  item,
  cataloguePlayback,
  catalogueArtists,
  catalogueContinue,
  catalogueUpNext,
  catalogueChildren as getCatalogueChildren,
} from './generated/kahawai.ts'
import type { ItemQuery } from './generated/model/itemQuery.ts'
import type { FeedItem } from './generated/model/feedItem.ts'
import type { LibraryChild } from './generated/model/libraryChild.ts'
import type { CatalogueChildrenParams } from './generated/model/catalogueChildrenParams.ts'
import type { CatalogueItem } from './generated/model/catalogueItem.ts'
import type { ItemSummary } from './catalogue-model.ts'
import type { ItemDetail } from './catalogue-model.ts'
import type { BrowseParams } from './catalogue-model.ts'

export function catalogueRow(entry: CatalogueItem, library: string): ItemSummary {
  return {
    id: entry.id,
    title: entry.title,
    year: entry.year ?? null,
    kind: entry.kind,
    library_id: library,
    artist: entry.artist ?? null,
    sources: entry.copy_ids.length,
    art_version: null,
    file_title: null,
    file_year: null,
    matched_title: null,
    match_confidence: entry.match_confidence ?? null,
    parent_id: null,
    parent_title: null,
    episode: null,
    episode_end: null,
    season: null,
    proj_episode: null,
    proj_season: null,
    played: entry.played ?? false,
    duration_ms: null,
    resume_duration_ms: entry.resume_duration_ms ?? null,
    resume_position_ms: entry.resume_position_ms ?? null,
    replay_gain: null,
  }
}

export async function listItems(params: BrowseParams = {}) {
  if (params.in_progress) {
    const result = await catalogueContinue({
      ...(params.library !== undefined ? { library: params.library } : {}),
      ...(params.limit !== undefined ? { limit: params.limit } : {}),
      ...(params.offset !== undefined ? { offset: params.offset } : {}),
    })
    return { ...result, items: result.items.map(feedRow) }
  }
  if (!params.library) throw new Error('Catalogue browsing requires a library.')
  const { library, ...page } = params
  const result = await items(library, page)
  return { ...result, items: result.items.map((entry) => catalogueRow(entry, library)) }
}

export async function listArtists(params: {
  library: string
  q?: string
  sort?: string
  offset?: number
  limit?: number
}) {
  const { library, ...page } = params
  return catalogueArtists(library, page)
}

export async function artistAlbums(
  key: string,
  params: { library: string; q?: string; sort?: string; offset?: number; limit?: number },
) {
  const { library, ...page } = params
  const { items: albums, ...result } = await items(library, { ...page, artist: key })
  return {
    ...result,
    albums: albums.map((entry) => catalogueRow(entry, library)),
    artist: { key, name: key, album_count: result.total },
  }
}

export async function catalogueDetail(
  library: string,
  id: string,
  query?: ItemQuery,
): Promise<ItemDetail> {
  const entry = query ? await cataloguePlayback(library, id, query) : await item(library, id)
  const row = entry.child
    ? childRow(entry.child, library, entry.parent_title)
    : catalogueRow(entry, library)
  return {
    ...row,
    played: entry.played,
    resume_position_ms: entry.resume_position_ms ?? null,
    resume_duration_ms: entry.resume_duration_ms ?? null,
    duration_ms: entry.duration_ms ?? null,
    chapters: entry.chapters,
    sources: entry.sources,
    copies: entry.copies,
    show_title: entry.child?.position.kind === 'episode' ? (entry.parent_title ?? null) : null,
    subtitle_source: entry.subtitle_source ?? null,
    negotiated: entry.negotiated ?? null,
    ...(entry.unavailable ? { unavailable: entry.unavailable } : {}),
    segments: entry.segments ?? [],
    metadata: entry.metadata.description,
    provider: entry.provider ?? null,
    tmdb_id: entry.tmdb_id ?? null,
    tvdb_id: entry.tvdb_id ?? null,
  }
}

function childRow(entry: LibraryChild, library: string, parentTitle?: string | null): ItemSummary {
  const position = entry.position
  return {
    ...catalogueRow(
      {
        id: entry.id,
        title: entry.title,
        media_type: position.kind === 'episode' ? 'series' : 'music',
        kind: position.kind === 'episode' ? 'series' : 'album',
        artist: entry.artist ?? null,
        year: null,
        copy_ids: [],
        representative_id: entry.representative_id,
        metadata: entry.metadata,
        played: false,
      },
      library,
    ),
    kind: position.kind === 'episode' ? 'episode' : 'song',
    sources: entry.source_count,
    parent_id: entry.parent_id,
    parent_title: parentTitle ?? null,
    season: position.kind === 'episode' ? (position.season ?? null) : (position.disc ?? null),
    episode:
      position.kind === 'episode'
        ? position.episode
        : position.kind === 'track'
          ? position.track
          : null,
  }
}

export async function catalogueChildren(
  library: string,
  id: string,
  params: CatalogueChildrenParams = {},
) {
  const page = await getCatalogueChildren(library, id, params)
  return {
    ...page,
    children: page.children.map((child) => ({
      ...childRow(child, library),
      ...page.watch?.[child.id],
    })),
  }
}

function feedRow(entry: FeedItem): ItemSummary {
  return {
    ...(entry.child
      ? childRow(entry.child, entry.library_id, entry.parent_title)
      : catalogueRow(entry, entry.library_id)),
    played: entry.played,
    resume_position_ms: entry.resume_position_ms ?? null,
    resume_duration_ms: entry.resume_duration_ms ?? null,
  }
}
export async function upNext(params: { library?: string; limit?: number; offset?: number } = {}) {
  const result = await catalogueUpNext(params)
  return { ...result, items: result.items.map(feedRow) }
}
