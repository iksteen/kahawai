import { beforeEach, expect, test, vi } from 'vitest'
import {
  items,
  item,
  cataloguePlayback,
  catalogueArtists,
  catalogueContinue,
  catalogueUpNext,
  catalogueChildren as children,
} from '../src/api/generated/kahawai.ts'
import {
  listItems,
  catalogueDetail,
  listArtists,
  catalogueRow,
  upNext,
  catalogueChildren,
} from '../src/api/catalogue.ts'
vi.mock('../src/api/generated/kahawai.ts', () => ({
  items: vi.fn(),
  item: vi.fn(),
  cataloguePlayback: vi.fn(),
  libraries: vi.fn(),
  catalogueArtists: vi.fn(),
  catalogueContinue: vi.fn(),
  catalogueUpNext: vi.fn(),
  catalogueChildren: vi.fn(),
}))
const entry = {
  negotiated: null,
  segments: [],
  played: false,
  sources: [],
  copies: [],
  chapters: [],
  id: 'film',
  media_type: 'movies',
  kind: 'movie' as const,
  title: 'Dark City',
  year: 1998,
  copy_ids: ['copy1', 'copy2'],
  representative_id: 'copy1',
  metadata: { description: { overview: 'A city without daylight.', rating: 7.5 } },
}
beforeEach(() => vi.resetAllMocks())
test.each(['auto', 'manual', 'weak', null])('grid preserves match confidence %s', (confidence) => {
  expect(catalogueRow({ ...entry, match_confidence: confidence }, 'films').match_confidence).toBe(
    confidence,
  )
})
test('the existing grids receive exact totals and scoped, filtered pages', async () => {
  vi.mocked(items).mockResolvedValue({ items: [entry], total: 101, limit: 50, offset: 50 })
  const page = await listItems({
    library: 'films',
    offset: 50,
    limit: 50,
    q: 'City',
    sort: '-year',
  })
  expect(items).toHaveBeenCalledWith('films', { offset: 50, limit: 50, q: 'City', sort: '-year' })
  expect(page.total).toBe(101)
  expect(page.items[0]).toMatchObject({
    id: 'film',
    kind: 'movie',
    library_id: 'films',
    sources: 2,
  })
})
test('the existing detail head receives metadata without claiming playable sources or legacy history', async () => {
  vi.mocked(item).mockResolvedValue(entry)
  const result = await catalogueDetail('films', 'film')
  expect(item).toHaveBeenCalledWith('films', 'film')
  expect(result.metadata).toMatchObject({ overview: 'A city without daylight.', rating: 7.5 })
  expect(result.sources).toEqual([])
  expect(result.resume_position_ms).toBeNull()
  expect(result.unavailable).toBeUndefined()
})
test('artist grids page through the catalogue API', async () => {
  vi.mocked(catalogueArtists).mockResolvedValue({ artists: [], total: 0, offset: 100, limit: 50 })
  await listArtists({ library: 'music', offset: 100, limit: 50, sort: '-name' })
  expect(catalogueArtists).toHaveBeenCalledWith('music', { offset: 100, limit: 50, sort: '-name' })
})

test('stable physical children are supplied by the catalogue, not metadata list positions', async () => {
  const child = {
    id: 'child1:parent:e:1:2',
    parent_id: 'parent',
    title: 'Second',
    position: { kind: 'episode' as const, season: 1, episode: 2 },
    representative_id: 'copy1',
    source_count: 2,
    metadata: { description: { overview: 'Second episode.' }, provenance: {} },
  }
  vi.mocked(children).mockResolvedValue({
    children: [child],
    total: 202,
    offset: 200,
    limit: 200,
    groups: [{ kind: 'episode', number: 1, total: 202, played: 1 }],
    watch: { [child.id]: { played: true } },
  })
  const page = await catalogueChildren('shows', 'parent', { offset: 200, season: '1' })
  expect(children).toHaveBeenCalledWith('shows', 'parent', { offset: 200, season: '1' })
  expect(page.total).toBe(202)
  expect(page.children[0]).toMatchObject({
    id: child.id,
    kind: 'episode',
    parent_id: 'parent',
    season: 1,
    episode: 2,
    sources: 2,
    played: true,
  })
  vi.mocked(item).mockResolvedValue({
    ...entry,
    id: child.id,
    child,
    parent_title: 'Show',
    metadata: child.metadata,
  })
  const result = await catalogueDetail('shows', child.id)
  expect(item).toHaveBeenCalledWith('shows', child.id)
  expect(result).toMatchObject({
    id: child.id,
    kind: 'episode',
    parent_id: 'parent',
    title: 'Second',
    show_title: 'Show',
  })
  expect(result.unavailable).toBeUndefined()
})

test('detail preserves physical sources, copy context, runtime and chapters', async () => {
  const source = {
    collection_item_id: 'copy1',
    module_id: 'host',
    host_name: 'Mediahost',
    collection_id: 'movies',
    path_rel: 'Dark.City.1998.mkv',
    size: 123,
    available: true,
    revision: 1,
    source_id: 1,
    part: 1,
    parts: 1,
  }
  const copy = {
    id: 'copy1',
    title: 'Dark City',
    paths: [source.path_rel],
    assignment: {
      collection_item_id: 'copy1',
      revision: 3,
      mode: 'auto',
      library_item_ids: ['film'],
    },
  }
  const chapters = [{ start_ms: 0, title: 'Opening' }]
  vi.mocked(item).mockResolvedValue({
    ...entry,
    sources: [source],
    copies: [copy],
    duration_ms: 90000,
    chapters,
  })
  const result = await catalogueDetail('films', 'film')
  expect(result.sources).toEqual([source])
  expect(result.copies).toEqual([copy])
  expect(result.duration_ms).toBe(90000)
  expect(result.chapters).toEqual(chapters)
  expect(result.negotiated).toBeNull()
})

test('catalogue watch state reaches movie and child detail presentation', async () => {
  vi.mocked(item).mockResolvedValue({
    ...entry,
    played: false,
    resume_position_ms: 1200,
    resume_duration_ms: 5000,
  })
  expect(await catalogueDetail('films', 'film')).toMatchObject({
    played: false,
    resume_position_ms: 1200,
    resume_duration_ms: 5000,
  })
  expect(catalogueRow({ ...entry, played: true }, 'films').played).toBe(true)
})

test('home feeds use catalogue routes and keep exact child links and resume state', async () => {
  const child = {
    id: 'child1:show:e:1:2',
    parent_id: 'show',
    title: 'Second',
    position: { kind: 'episode' as const, season: 1, episode: 2 },
    representative_id: 'copy1',
    source_count: 1,
    metadata: { description: {}, provenance: {} },
  }
  const row = {
    ...entry,
    id: child.id,
    title: child.title,
    library_id: 'shows',
    parent_title: 'Show',
    child,
    resume_position_ms: 120000,
    resume_duration_ms: 600000,
  }
  const page = { items: [row], total: 1, offset: 0, limit: 12 }
  vi.mocked(catalogueContinue).mockResolvedValue(page)
  vi.mocked(catalogueUpNext).mockResolvedValue(page)
  const progress = await listItems({ in_progress: true, limit: 12 })
  expect(catalogueContinue).toHaveBeenCalledWith({
    limit: 12,
    library: undefined,
    offset: undefined,
  })
  expect(progress.items[0]).toMatchObject({
    id: child.id,
    parent_id: 'show',
    library_id: 'shows',
    kind: 'episode',
    season: 1,
    episode: 2,
    resume_position_ms: 120000,
  })
  const next = await upNext({ limit: 12 })
  expect(catalogueUpNext).toHaveBeenCalledWith({ limit: 12 })
  expect(next.items[0]!.id).toBe(child.id)
})

test('playback previews use the scoped catalogue API and preserve its refusal', async () => {
  const unavailable = {
    code: 'source_offline' as const,
    message: 'Source is offline',
    request_id: 'request',
  }
  vi.mocked(cataloguePlayback).mockResolvedValue({ ...entry, unavailable })
  const query = {
    profile: { containers: ['mp4'], target_duration: { mode: 'ignore' as const } },
    media_entry_id: 'rendition-b',
  }
  const result = await catalogueDetail('films', 'film', query)
  expect(cataloguePlayback).toHaveBeenCalledWith('films', 'film', query)
  expect(item).not.toHaveBeenCalled()
  expect(result.unavailable).toEqual(unavailable)
})

test.each(['movie', 'series'] as const)('anime preserves its %s shape in the grid', (kind) => {
  expect(catalogueRow({ ...entry, media_type: 'anime', kind }, 'anime').kind).toBe(kind)
})

test('catalogue detail preserves provider IDs for the client community lookup', async () => {
  vi.mocked(item).mockResolvedValue({ ...entry, tmdb_id: 2666, tvdb_id: 123 } as never)
  const result = await catalogueDetail('films', 'film')
  expect(result.metadata).toMatchObject({ tmdb_id: 2666, tvdb_id: 123 })
})
