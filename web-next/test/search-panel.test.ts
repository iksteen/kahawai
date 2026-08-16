/// The search panel, mounted. Two rules run through all of it: a library that
/// could not be asked is not a library with no matches, and the rows that are
/// on screen stay actionable while their replacements load.

import { QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { defineComponent, h, ref } from 'vue'

import type { ItemRowI64 } from '../src/api/generated/model/itemRowI64.ts'
import { ApiError } from '../src/api/errors.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  listItems: vi.fn(),
  getItemArtworkUrl: (id: string) => `/api/v1/items/${id}/artwork`,
}))

const { listItems } = await import('../src/api/generated/kahawai.ts')
const { useSearchPanel } = await import('../src/composables/search-panel.ts')
const { notice, clearNotices } = await import('../src/composables/notices.ts')

const films = { id: 'films', name: 'Films', media_type: 'movies' }
const music = { id: 'music', name: 'Music', media_type: 'music' }
const item = (id: string) => ({ id, title: id, kind: 'movie' }) as ItemRowI64
const page = (items: ItemRowI64[], total = items.length) => ({ items, total, limit: 5, offset: 0 })

/// The composable in a component, because it uses queries and a scope.
function panel(query = ref('heat'), libraries = ref([films, music])) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  let api!: ReturnType<typeof useSearchPanel>
  mount(
    defineComponent({
      setup() {
        api = useSearchPanel(libraries, query)
        return () => h('div')
      },
    }),
    { global: { plugins: [[VueQueryPlugin, { queryClient: client }]] } },
  )
  return { api: () => api, query, libraries }
}

/// Each library answers with what it is given, or refuses.
function answers(by: Record<string, ItemRowI64[] | 'fails'>) {
  vi.mocked(listItems).mockImplementation(async (params) => {
    const answer = by[params?.library ?? '']
    if (answer === 'fails' || answer === undefined) throw new ApiError(503, 'the hub is restarting')
    return page(answer)
  })
}

beforeEach(() => clearNotices())
afterEach(() => vi.resetAllMocks())

describe('asking', () => {
  test('every library at once, five each', async () => {
    answers({ films: [item('a')], music: [item('b')] })
    const { api } = panel()
    await flushPromises()
    expect(listItems).toHaveBeenCalledTimes(2)
    expect(listItems).toHaveBeenCalledWith({ library: 'films', q: 'heat', limit: 5 })
    // A heading and a hit, twice.
    expect(api().rows.value.map((r) => r.kind)).toEqual(['library', 'item', 'library', 'item'])
  })

  test('nothing at all with an empty box', async () => {
    answers({ films: [item('a')], music: [item('b')] })
    const { api } = panel(ref(''))
    await flushPromises()
    expect(listItems).not.toHaveBeenCalled()
    expect(api().drawn.value).toBe(false)
  })

  test('and nothing while the library list is still coming', async () => {
    // An empty list searched nothing and answered immediately, and the panel
    // stated "No matches" as a fact about the catalogue — from a search that
    // never ran, with no Try again, because nothing failed from its point of
    // view.
    answers({})
    const { api } = panel(ref('heat'), ref([]))
    await flushPromises()
    expect(listItems).not.toHaveBeenCalled()
    expect(api().drawn.value).toBe(false)
  })
})

describe('a library that could not be asked', () => {
  test('is named, and does not read as no matches', async () => {
    answers({ films: [item('a')], music: 'fails' })
    const { api } = panel()
    await flushPromises()
    expect(api().failed.value).toEqual(['Music'])
    // Films still shows what it found.
    expect(api().rows.value.some((r) => r.kind === 'item')).toBe(true)
  })

  test('and one notice covers the whole search rather than one per library', async () => {
    // Notices are latest-wins: one each would name whichever failed last and
    // imply the rest were fine.
    answers({ films: 'fails', music: 'fails' })
    panel()
    await flushPromises()
    expect(notice.value).toContain('the hub is restarting')
  })

  test('the notice names the libraries when only some failed', async () => {
    answers({ films: [item('a')], music: 'fails' })
    panel()
    await flushPromises()
    expect(notice.value).toBe('Could not search Music.')
  })

  test('and asking again is offered', async () => {
    answers({ films: 'fails', music: 'fails' })
    const { api } = panel()
    await flushPromises()
    expect(api().failed.value).toHaveLength(2)

    answers({ films: [item('a')], music: [item('b')] })
    api().retry()
    await flushPromises()
    expect(api().failed.value).toEqual([])
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
    vi.mocked(listItems).mockReturnValue(
      new Promise((resolve) => (release = () => resolve(page([item('z')])))),
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
