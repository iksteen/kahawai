vi.mock('../src/api/catalogue.ts', () => ({
  listItems: vi.fn(),
  listArtists: vi.fn(),
  listTracks: vi.fn(),
  searchEverywhere: vi.fn(),
  artistAlbums: vi.fn(),
  upNext: vi.fn(),
  catalogueDetail: vi.fn(),
  catalogueChildren: vi.fn(),
}))
/// The search panel, mounted. Two rules run through all of it: a library that
/// could not be asked is not a library with no matches, and the rows that are
/// on screen stay actionable while their replacements load.

import { QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { defineComponent, h, ref } from 'vue'

import type { ItemSummary } from '../src/api/catalogue-model.ts'
import { ApiError } from '../src/api/errors.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  libraries: vi.fn(),
  getCatalogueArtworkUrl: (library: string, id: string) =>
    `/api/v1/catalogue/libraries/${library}/items/${id}/artwork`,
}))

const { searchEverywhere } = await import('./api-fixture.ts')
const { useSearchPanel } = await import('../src/composables/search-panel.ts')
const { notice, clearNotices } = await import('../src/composables/notices.ts')

const films = { id: 'films', name: 'Films', media_type: 'movies' }
const music = { id: 'music', name: 'Music', media_type: 'music' }
const item = (id: string) => ({ id, title: id, kind: 'movie' }) as ItemSummary
type Hits = Awaited<ReturnType<typeof searchEverywhere>>[number]
/// One library's answer: its items, and nothing of the other kinds unless a
/// test says so.
const hits = (library: typeof films, items: ItemSummary[], more: Partial<Hits> = {}): Hits => ({
  library,
  items,
  total: items.length,
  artists: [],
  artistTotal: 0,
  songs: [],
  songTotal: 0,
  ...more,
})

/// The composable in a component, because it uses queries and a scope.
function panel(query = ref('heat')) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  let api!: ReturnType<typeof useSearchPanel>
  mount(
    defineComponent({
      setup() {
        api = useSearchPanel(query)
        return () => h('div')
      },
    }),
    { global: { plugins: [[VueQueryPlugin, { queryClient: client }]] } },
  )
  return { api: () => api, query }
}

/// The hub answers Films then Music with what they are given — or, with
/// 'fails', refuses the whole search.
function answers(by: { films?: ItemSummary[]; music?: ItemSummary[] } | 'fails') {
  vi.mocked(searchEverywhere).mockImplementation(async () => {
    if (by === 'fails') throw new ApiError(503, 'the hub is restarting')
    return [hits(films, by.films ?? []), hits(music, by.music ?? [])]
  })
}

beforeEach(() => {
  clearNotices()
})
afterEach(() => vi.resetAllMocks())

describe('asking', () => {
  test('every library at once, in one request, five each', async () => {
    answers({ films: [item('a')], music: [item('b')] })
    const { api } = panel()
    await flushPromises()
    expect(searchEverywhere).toHaveBeenCalledTimes(1)
    expect(searchEverywhere).toHaveBeenCalledWith({ q: 'heat', limit: 5 })
    // A heading and a hit, twice.
    expect(api().rows.value.map((r) => r.kind)).toEqual(['library', 'item', 'library', 'item'])
  })

  test('a music library answers with its album artists and songs as well', async () => {
    const song = { ...item('child1:x:t:u:1'), kind: 'song', title: 'Heat of the Moment' }
    vi.mocked(searchEverywhere).mockResolvedValue([
      hits(films, [item('heat')]),
      hits(music, [item('Heat Album')], {
        artists: [{ key: 'heatwave', name: 'Heatwave', album_count: 3 }],
        artistTotal: 2,
        songs: [song],
        songTotal: 7,
      }),
    ])
    const { api } = panel()
    await flushPromises()
    const rows = api().rows.value
    expect(rows.map((r) => r.kind)).toEqual([
      'library',
      'item',
      'library',
      'artist',
      'item',
      'item',
    ])
    // Artists, then albums, then songs — and the heading counts all three.
    expect(rows[4]).toMatchObject({ kind: 'item', item: { title: 'Heat Album' } })
    expect(rows[5]).toMatchObject({ kind: 'item', item: { kind: 'song' } })
    expect(rows[2]).toMatchObject({ kind: 'library', shown: 3, total: 2 + 1 + 7 })
  })

  test('a music library with only song hits still gets its heading', async () => {
    const song = { ...item('child1:x:t:u:1'), kind: 'song' }
    vi.mocked(searchEverywhere).mockResolvedValue([
      hits(films, []),
      hits(music, [], { songs: [song], songTotal: 1 }),
    ])
    const { api } = panel()
    await flushPromises()
    expect(api().rows.value.map((r) => r.kind)).toEqual(['library', 'item'])
  })

  test('nothing at all with an empty box', async () => {
    answers({ films: [item('a')], music: [item('b')] })
    const { api } = panel(ref(''))
    await flushPromises()
    expect(searchEverywhere).not.toHaveBeenCalled()
    expect(api().drawn.value).toBe(false)
  })
})

describe('a search that could not be answered', () => {
  test('says so, and shows no rows at all', async () => {
    // All or nothing: results with a library quietly missing read as fewer
    // matches.
    answers('fails')
    const { api } = panel()
    await flushPromises()
    expect(api().failure.value).toBe('the hub is restarting')
    expect(api().rows.value).toEqual([])
    // Still a panel on screen — the failure and its Try again are in it.
    expect(api().drawn.value).toBe(true)
  })

  test('not even the rows of the search before it', async () => {
    answers({ films: [item('a')] })
    const query = ref('heat')
    const { api } = panel(query)
    await flushPromises()
    expect(api().rows.value).toHaveLength(2)

    answers('fails')
    query.value = 'other'
    await flushPromises()
    expect(api().failure.value).not.toBe('')
    expect(api().rows.value).toEqual([])
    expect(api().shownQuery.value).toBe('other')
  })

  test('and one notice says so', async () => {
    answers('fails')
    panel()
    await flushPromises()
    expect(notice.value).toBe('Could not search — the hub is restarting')
  })

  test('and a failure that repeats is reported every time', async () => {
    // The de-dup remembered the sentence it had said and refused to repeat it.
    // Notices clear after five seconds, so there was nothing on screen to
    // duplicate: pressing Try again against a hub that was still down said
    // nothing at all.
    answers('fails')
    const { api } = panel()
    await flushPromises()
    expect(notice.value).not.toBe('')

    clearNotices()
    api().retry()
    await flushPromises()
    expect(notice.value).not.toBe('')
  })

  test('and its banner does not sit over the next search while that is asked', async () => {
    answers('fails')
    const query = ref('heat')
    const { api } = panel(query)
    await flushPromises()
    expect(api().failure.value).not.toBe('')

    let answer = () => {}
    vi.mocked(searchEverywhere).mockReturnValue(
      new Promise((resolve) => (answer = () => resolve([]))),
    )
    query.value = 'other'
    await flushPromises()
    expect(api().searching.value).toBe(true)
    expect(api().failure.value).toBe('')

    answer()
    await flushPromises()
  })

  test('emptying the box stops the search, even mid-flight', async () => {
    // Otherwise the panel is "still searching" for a query nobody typed, and
    // the failure banner it is holding back never comes back.
    let answer = () => {}
    vi.mocked(searchEverywhere).mockReturnValue(
      new Promise((resolve) => (answer = () => resolve([]))),
    )
    const query = ref('heat')
    const { api } = panel(query)
    await flushPromises()
    expect(api().searching.value).toBe(true)

    query.value = ''
    await flushPromises()
    expect(api().searching.value).toBe(false)
    expect(api().drawn.value).toBe(false)

    answer()
    await flushPromises()
  })

  test('and asking again is offered', async () => {
    answers('fails')
    const { api } = panel()
    await flushPromises()
    expect(api().failure.value).not.toBe('')

    answers({ films: [item('a')], music: [item('b')] })
    api().retry()
    await flushPromises()
    expect(api().failure.value).toBe('')
    expect(api().rows.value).toHaveLength(4)
  })
})

describe('while the next answer is on its way', () => {
  test('the rows already on screen stay', async () => {
    // They are visible, so pressing Enter on their highlight has to remain
    // predictable; blanking the panel makes every debounced keystroke flash
    // the whole surface away.
    answers({ films: [item('a')], music: [] })
    const query = ref('heat')
    const { api } = panel(query)
    await flushPromises()
    expect(api().rows.value).toHaveLength(2)

    let release = () => {}
    vi.mocked(searchEverywhere).mockReturnValue(
      new Promise((resolve) => (release = () => resolve([hits(films, [item('z')])]))),
    )
    query.value = 'heat 2'
    await flushPromises()
    expect(api().searching.value).toBe(true)
    expect(api().rows.value).toHaveLength(2)

    release()
    await flushPromises()
    expect(api().rows.value.some((r) => r.kind === 'item' && r.item.id === 'z')).toBe(true)
  })

  test('and the panel counts as drawn, because there is something on it', async () => {
    // `drawn` is what the input's `aria-expanded` reports. A query that
    // matched nothing puts a visible panel on screen with no rows in it, and
    // deriving this from the row count told a screen reader the combobox was
    // collapsed while "No matches" was showing.
    answers({ films: [], music: [] })
    const { api } = panel()
    await flushPromises()
    expect(api().rows.value).toHaveLength(0)
    expect(api().drawn.value).toBe(true)
  })
})

describe('the highlight', () => {
  test('starts on nothing', async () => {
    answers({ films: [item('a')], music: [] })
    const { api } = panel()
    await flushPromises()
    expect(api().highlight.value).toBe(-1)
  })

  test('and is dropped when the rows are replaced', async () => {
    // A position kept in a list that has been replaced points at a different
    // film, and Enter would open it.
    answers({ films: [item('a'), item('b')], music: [] })
    const query = ref('heat')
    const { api } = panel(query)
    await flushPromises()
    api().highlight.value = 2

    answers({ films: [item('c')], music: [] })
    query.value = 'other'
    await flushPromises()
    expect(api().highlight.value).toBe(-1)
  })
})
