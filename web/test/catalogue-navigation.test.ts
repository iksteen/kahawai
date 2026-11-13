import { beforeEach, expect, test, vi } from 'vitest'
import {
  item,
  cataloguePlayback,
  catalogueSearch,
  catalogueContinue,
  catalogueUpNext,
  catalogueChildren as children,
  catalogueSeasonItems as seasonItems,
} from '../src/api/generated/kahawai.ts'
import {
  listItems,
  catalogueDetail,
  listArtists,
  catalogueRow,
  upNext,
  catalogueChildren,
  searchEverywhere,
  listTracks,
  artistAlbums,
} from '../src/api/catalogue.ts'
vi.mock('../src/api/generated/kahawai.ts', () => ({
  item: vi.fn(),
  cataloguePlayback: vi.fn(),
  libraries: vi.fn(),
  catalogueSearch: vi.fn(),
  catalogueContinue: vi.fn(),
  catalogueUpNext: vi.fn(),
  catalogueChildren: vi.fn(),
  catalogueSeasonItems: vi.fn(),
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
  metadata: { description: { overview: 'A city without daylight.', rating: 7.5 }, provenance: {} },
}
beforeEach(() => vi.resetAllMocks())
test.each(['auto', 'manual', 'weak', null])('grid preserves match confidence %s', (confidence) => {
  expect(catalogueRow({ ...entry, match_confidence: confidence }, 'films').match_confidence).toBe(
    confidence,
  )
})
/// `/search` answering one library with the parts given.
const answer = (library: string, parts: Record<string, unknown>) =>
  vi.mocked(catalogueSearch).mockResolvedValue({
    libraries: [{ library: { id: library, name: library, media_type: 'music' }, ...parts }],
  } as never)

test('the existing grids receive exact totals and scoped, filtered pages', async () => {
  answer('films', { items: { items: [entry], total: 101, limit: 50, offset: 50 } })
  const page = await listItems({
    library: 'films',
    offset: 50,
    limit: 50,
    q: 'City',
    sort: '-year',
  })
  // One library, one kind: the page that grid asks for.
  expect(catalogueSearch).toHaveBeenCalledWith({
    libraries: 'films',
    kinds: 'items',
    offset: 50,
    limit: 50,
    q: 'City',
    sort: '-year',
  })
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
  answer('music', { artists: { artists: [], total: 0, offset: 100, limit: 50 } })
  await listArtists({ library: 'music', offset: 100, limit: 50, sort: '-name' })
  expect(catalogueSearch).toHaveBeenCalledWith({
    libraries: 'music',
    kinds: 'artists',
    offset: 100,
    limit: 50,
    sort: '-name',
  })
})

test('an artist page asks for that artist’s items, and nothing undefined', async () => {
  answer('music', { items: { items: [entry], total: 1, offset: 0, limit: 100 } })
  const page = await artistAlbums('Asia', { library: 'music', limit: 100 })
  expect(catalogueSearch).toHaveBeenCalledWith({
    libraries: 'music',
    kinds: 'items',
    limit: 100,
    artist: 'Asia',
  })
  expect(page.albums[0]).toMatchObject({ id: 'film', library_id: 'music' })
})

test('songs page as rows that open their album', async () => {
  answer('music', {
    tracks: {
      tracks: [
        {
          id: 'child1:album:t:u:3',
          parent_id: 'album',
          album: 'Asia',
          position: { kind: 'track', disc: null, track: 3 },
          title: 'Heat of the Moment',
          artist: 'Asia',
        },
      ],
      total: 1,
      offset: 0,
      limit: 100,
    },
  })
  const page = await listTracks({ library: 'music', q: 'heat', limit: 100 })
  expect(catalogueSearch).toHaveBeenCalledWith({
    libraries: 'music',
    kinds: 'tracks',
    q: 'heat',
    limit: 100,
  })
  expect(page.items[0]).toMatchObject({
    kind: 'song',
    parent_id: 'album',
    episode: 3,
    season: null,
  })
})

test('a library answered without the kind asked for is a contract break, not no matches', async () => {
  answer('music', { artists: null })
  await expect(listArtists({ library: 'music' })).rejects.toThrow('without artists')
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
  vi.mocked(seasonItems).mockResolvedValue({
    children: [child],
    total: 202,
    offset: 200,
    limit: 200,
    groups: [{ kind: 'episode', number: 1, total: 202, played: 1 }],
    watch: { [child.id]: { played: true } },
  })
  const page = await catalogueChildren('shows', 'parent', { offset: 200, season: '1' })
  expect(seasonItems).toHaveBeenCalledWith('shows', 'parent', '1', {
    offset: 200,
    limit: undefined,
  })
  expect(children).not.toHaveBeenCalled()
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
  expect(result).toMatchObject({ tmdb_id: 2666, tvdb_id: 123 })
})

test('catalogue detail uses the generated description without remapping fields', async () => {
  const description = {
    overview: 'A city without daylight.',
    release_date: '1998-02-27',
    genres: [],
    cast: [{ name: 'Rufus Sewell', role: 'John Murdoch' }],
    original_title: 'Dark City',
    original_language: 'en',
    artwork: null,
  }
  vi.mocked(item).mockResolvedValue({
    ...entry,
    metadata: {
      description,
      provenance: { overview: 'provider-record' },
      providers: { 'provider-record': 'tmdb', 'primary-record': 'tvdb' },
    },
    provider: 'tmdb',
    match_confidence: 'weak',
  })
  const result = await catalogueDetail('films', 'film')
  expect(result.metadata).toBe(description)
  expect(result.attribution).toEqual(['tmdb', 'tvdb'])
  expect(result.provider).toBe('tmdb')
  expect(result.match_confidence).toBe('weak')
})

test.each(['0', 'absolute'])('season item URLs preserve the %s selector', async (season) => {
  vi.mocked(seasonItems).mockResolvedValue({
    children: [],
    total: 0,
    offset: 0,
    limit: 200,
    groups: [],
    watch: {},
  })
  await catalogueChildren('shows', 'parent', { season, limit: 200 })
  expect(seasonItems).toHaveBeenCalledWith('shows', 'parent', season, {
    offset: undefined,
    limit: 200,
  })
  expect(children).not.toHaveBeenCalled()
})

test('season and disc filters cannot be combined', async () => {
  await expect(catalogueChildren('shows', 'parent', { season: '1', disc: '1' })).rejects.toThrow(
    'Season and disc filters cannot be combined.',
  )
  expect(seasonItems).not.toHaveBeenCalled()
  expect(children).not.toHaveBeenCalled()
})

test('one search answers every library, each with the kinds it holds', async () => {
  vi.mocked(catalogueSearch).mockResolvedValue({
    libraries: [
      {
        library: { id: 'films', name: 'Films', media_type: 'movies' },
        items: { items: [entry], total: 3, limit: 5, offset: 0 },
        artists: null,
        tracks: null,
      },
      {
        library: { id: 'music', name: 'Music', media_type: 'music' },
        items: { items: [], total: 0, limit: 5, offset: 0 },
        artists: {
          artists: [{ key: 'asia', name: 'Asia', album_count: 2 }],
          total: 1,
          limit: 5,
          offset: 0,
        },
        tracks: {
          tracks: [
            {
              id: 'child1:album:t:1:3',
              parent_id: 'album',
              album: 'Asia',
              position: { kind: 'track', disc: 1, track: 3 },
              title: 'Heat of the Moment',
              artist: 'Asia',
            },
          ],
          total: 9,
          limit: 5,
          offset: 0,
        },
      },
    ],
  })
  const [films, music] = await searchEverywhere({ q: 'heat', limit: 5 })
  if (!films || !music) throw new Error('expected both libraries')
  expect(catalogueSearch).toHaveBeenCalledWith({ q: 'heat', limit: 5 })
  expect(films).toMatchObject({ total: 3, artists: [], artistTotal: 0, songs: [], songTotal: 0 })
  expect(films.items[0]).toMatchObject({ id: 'film', library_id: 'films' })
  expect(music).toMatchObject({ library: { id: 'music' }, artistTotal: 1, songTotal: 9 })
  expect(music.artists[0]?.name).toBe('Asia')
  expect(music.songs[0]).toMatchObject({
    kind: 'song',
    id: 'child1:album:t:1:3',
    library_id: 'music',
    parent_id: 'album',
    parent_title: 'Asia',
    season: 1,
    episode: 3,
  })
})
