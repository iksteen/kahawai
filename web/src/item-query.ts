import { buildProfile } from './capabilities'
import { itemQuery } from './generated/kahawai'
import type { ItemQueryResponse } from './generated/model'
import type { Item, ItemDetail, Source } from './api'

/// Ask what this client would actually be served (RFC 10008 QUERY).
/// The profile is the unrefined browser probe: session start still refines it
/// against the announced streams. QUERY returns those streams as `sources`;
/// the existing view model keeps both their count and full detail.
export async function fetchItem(id: string): Promise<ItemDetail> {
  const raw: ItemQueryResponse = await itemQuery(id, { profile: buildProfile() })
  const sources = raw.sources.map((source) => ({
    ...source,
    streams: source.streams ?? null,
  })) as Source[]
  return {
    ...(raw as unknown as Item),
    negotiated: raw.negotiated ?? undefined,
    metadata: raw.metadata ?? undefined,
    sources: sources.length,
    sources_detail: sources,
  } as ItemDetail
}
