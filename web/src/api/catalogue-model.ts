// View models used by the catalogue presentation adapter. Wire types remain generated.
import type { ClientReplayGain } from './generated/model/clientReplayGain.ts'
import type { CatalogueDetail } from './generated/model/catalogueDetail.ts'
import type { ItemsParams } from './generated/model/itemsParams.ts'

export type ItemSummary = {
  art_version: number | null
  artist: string | null
  duration_ms: number | null
  episode: number | null
  episode_end: number | null
  file_title: string | null
  file_year: number | null
  id: string
  kind: string
  library_id: string | null
  match_confidence: string | null
  matched_title: string | null
  parent_id: string | null
  parent_title: string | null
  played: boolean
  premiered: string | null
  proj_episode: number | null
  proj_season: number | null
  replay_gain: ClientReplayGain | null
  resume_duration_ms: number | null
  resume_position_ms: number | null
  season: number | null
  sources: number
  title: string
  year: number | null
}

export type ItemDescription = {
  cast:
    | {
        character: string | null
        name: string
      }[]
    | null
  confidence: string
  genres: string[] | null
  original_language: string | null
  overview: string | null
  premiered: string | null
  proj_episode: number | null
  proj_season: number | null
  provider: string | null
  rating: number | null
  tmdb_id: number | null
  tvdb_id: number | null
}

export type ItemDetail = Omit<ItemSummary, 'sources'> &
  Pick<
    CatalogueDetail,
    | 'sources'
    | 'copies'
    | 'chapters'
    | 'negotiated'
    | 'unavailable'
    | 'subtitle_source'
    | 'segments'
  > & {
    related?: { kind: string; title: string | null; item_id: string | null }[]
    show_title: string | null
    metadata?: ItemDescription | null
  }
export type BrowseParams = ItemsParams & { library?: string; in_progress?: boolean }
