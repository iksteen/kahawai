/// The home screen, mounted. The rule most of this is about: a library that
/// would not load must not look like a library with nothing in it — the second
/// is dropped, and conflating them deleted whole libraries from this screen
/// with nothing said.

import { QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { defineComponent, h, ref } from 'vue'
import { createMemoryHistory, createRouter } from 'vue-router'

import type { ItemRowI64 } from '../src/api/generated/model/itemRowI64.ts'
import { ApiError } from '../src/api/errors.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  listLibraries: vi.fn(),
  listItems: vi.fn(),
  getItemArtworkUrl: (id: string) => `/api/v1/items/${id}/artwork`,
}))
vi.mock('../src/api/session.ts', () => ({ whoAmI: vi.fn(() => ({ username: 'x', admin: false })) }))

const { listItems, listLibraries } = await import('../src/api/generated/kahawai.ts')
const { whoAmI } = await import('../src/api/session.ts')
const { clearNotices, notice } = await import('../src/composables/notices.ts')
const Home = (await import('../src/views/Home.vue')).default
const { useShelves } = await import('../src/composables/home.ts')

const LIBS = [
  { id: 'films', name: 'Films', media_type: 'movies' },
  { id: 'music', name: 'Music', media_type: 'music' },
]

const item = (id: string, over: Partial<ItemRowI64> = {}) =>
  ({
    id,
    title: id,
    kind: 'movie',
    played: false,
    art_version: null,
    library_id: 'films',
    parent_id: null,
    parent_title: null,
    artist: null,
    year: null,
    season: null,
    episode: null,
    episode_end: null,
    resume_position_ms: null,
    resume_duration_ms: null,
    ...over,
  }) as ItemRowI64

/// No retries and no cache between tests: a retrying client turns one refused
/// request into three and a shared cache carries an answer into the next test.
function home() {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/', name: 'libraries', component: Home },
      { path: '/library/:library', name: 'library', component: { template: '<div />' } },
      {
        path: '/library/:library/item/:id',
        name: 'detail',
        component: { template: '<div />' },
      },
    ],
  })
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  return {
    router,
    wrapper: mount(Home, {
      global: { plugins: [router, [VueQueryPlugin, { queryClient: client }]] },
    }),
  }
}

/// A page of items, with the paging echo the hub sends back.
const page = (items: ItemRowI64[], total = items.length, offset = 0) => ({
  items,
  total,
  limit: 20,
  offset,
})

/// The shelves answer one library and refuse the other.
///
/// Note what happens on mount: happy-dom reports every measurement as zero, so
/// every lane is "near its end" the moment it exists and each shelf asks for a
/// second page straight away. That is correct behaviour — a lane whose cards
/// fit should fill itself — but it means a 25-item fixture is 25 on screen by
/// the time a test looks, not 20.
function shelvesThat(answers: Record<string, ItemRowI64[] | 'fails'>, { pagesFail = false } = {}) {
  vi.mocked(listItems).mockImplementation(async (params) => {
    if (params?.in_progress) return page([])
    const answer = answers[params?.library ?? '']
    if (answer === 'fails' || answer === undefined) throw new ApiError(500, 'no')
    const from = params?.offset ?? 0
    if (pagesFail && from > 0) throw new ApiError(500, 'no')
    return page(answer.slice(from, from + (params?.limit ?? 20)), answer.length, from)
  })
}

beforeEach(() => {
  vi.mocked(listLibraries).mockResolvedValue({ libraries: LIBS })
  vi.mocked(whoAmI).mockReturnValue({ username: 'x', admin: false })
  clearNotices()
})
afterEach(() => vi.resetAllMocks())

describe('before anything has answered', () => {
  test('every library already has a row of ghosts', async () => {
    // The page has its shape before any content arrives; on a slow link that
    // is the difference between a page and a blank. Nothing is judged empty
    // and nothing is judged failed until it has answered.
    let answer = () => {}
    vi.mocked(listItems).mockReturnValue(
      new Promise((resolve) => (answer = () => resolve(page([])))),
    )
    const { wrapper } = home()
    await flushPromises()

    expect(wrapper.text()).toContain('Films')
    expect(wrapper.text()).toContain('Music')
    expect(wrapper.text()).not.toContain('This one would not load.')
    // The point of the ghosts is that the page has the shape it will have:
    // eight cards per shelf, one shelf per library. One ghost, or ghosts for
    // only one of the two libraries, is a page that still jumps.
    const lanes = wrapper
      .findAll('[aria-hidden="true"]')
      .filter((el) => el.findAll('.ghost-art').length > 0)
    expect(lanes).toHaveLength(2)
    for (const lane of lanes) expect(lane.findAll('.ghost-art')).toHaveLength(8)
    // Both shelves say they are working, which is the only thing a screen
    // reader can be told about a row of decorative blanks.
    expect(wrapper.findAll('[aria-busy="true"]')).toHaveLength(2)
    // And no count, because there is nothing to count yet.
    expect(wrapper.text()).not.toContain(' of ')
    answer()
    await flushPromises()
  })
})

describe('a library that would not load', () => {
  test('keeps its place and says so, where an empty one is dropped', async () => {
    shelvesThat({ films: 'fails', music: [] })
    const { wrapper } = home()
    await flushPromises()

    expect(wrapper.text()).toContain('Films')
    expect(wrapper.text()).toContain('This one would not load.')
    // Music answered and had nothing: an empty rail under a heading reads as a
    // failure to load, so there is no heading.
    expect(wrapper.text()).not.toContain('Music')
  })

  test('and can be asked again without disturbing the others', async () => {
    shelvesThat({ films: 'fails', music: [item('m1', { library_id: 'music' })] })
    const { wrapper } = home()
    await flushPromises()
    expect(wrapper.text()).toContain('This one would not load.')

    shelvesThat({ films: [item('f1')], music: [item('m1', { library_id: 'music' })] })
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Try again')!
      .trigger('click')
    await flushPromises()

    expect(wrapper.text()).not.toContain('This one would not load.')
    expect(wrapper.text()).toContain('f1')
    expect(wrapper.text()).toContain('m1')
  })

  test('and a second failure gives the button back rather than ghosting on', async () => {
    // A row that silently keeps ghosting is the vanishing shelf again in a
    // different costume.
    shelvesThat({ films: 'fails', music: [] })
    const { wrapper } = home()
    await flushPromises()
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Try again')!
      .trigger('click')
    await flushPromises()
    expect(wrapper.findAll('button').some((b) => b.text() === 'Try again')).toBe(true)
  })
})

describe('the libraries themselves', () => {
  test('failing is the whole screen, because there is no home screen without them', async () => {
    vi.mocked(listLibraries).mockRejectedValue(new ApiError(503, 'restarting'))
    shelvesThat({})
    const { wrapper } = home()
    await flushPromises()
    expect(wrapper.text()).toContain('Could not load your libraries.')
    expect(wrapper.text()).toContain('restarting')
  })

  test('and none of them is not a failure', async () => {
    vi.mocked(listLibraries).mockResolvedValue({ libraries: [] })
    shelvesThat({})
    const { wrapper } = home()
    await flushPromises()
    expect(wrapper.text()).not.toContain('Could not load')
    expect(wrapper.text()).toContain('Ask whoever runs this hub')
  })

  test('which says something different to whoever runs the hub', async () => {
    vi.mocked(listLibraries).mockResolvedValue({ libraries: [] })
    vi.mocked(whoAmI).mockReturnValue({ username: 'x', admin: true })
    shelvesThat({})
    const { wrapper } = home()
    await flushPromises()
    expect(wrapper.text()).toContain('mediahost')
  })
})

describe('continue watching', () => {
  test('is a row when there is something on the go', async () => {
    vi.mocked(listItems).mockImplementation(async (params) => {
      if (params?.in_progress) {
        return page([item('half', { resume_position_ms: 60, resume_duration_ms: 120 })])
      }
      return page([])
    })
    const { wrapper } = home()
    await flushPromises()
    expect(wrapper.text()).toContain('Continue watching')
    expect(wrapper.text()).toContain('half')
  })

  test('and no row at all when there is not', async () => {
    shelvesThat({ films: [], music: [] })
    const { wrapper } = home()
    await flushPromises()
    expect(wrapper.text()).not.toContain('Continue watching')
  })

  test('and a row that would not load is reported, not simply absent', async () => {
    // No row at all reads as "you have nothing on the go" — the same lie the
    // shelves tell when they fail, and louder, because the row is not there to
    // be doubted.
    vi.mocked(listItems).mockImplementation(async (params) => {
      if (params?.in_progress) throw new ApiError(500, 'no')
      return page([])
    })
    const { wrapper } = home()
    await flushPromises()
    expect(wrapper.text()).not.toContain('Continue watching')
    expect(notice.value).toBe('Could not load what you were watching.')
  })

  test('an item in no library is not offered, because it has no page', async () => {
    // The URL is /library/{id}/item/{id}; only an unrestricted account can see
    // one at all.
    vi.mocked(listItems).mockImplementation(async (params) => {
      if (params?.in_progress) {
        return page([item('orphan', { library_id: null }), item('fine')])
      }
      return page([])
    })
    const { wrapper } = home()
    await flushPromises()
    expect(wrapper.text()).toContain('fine')
    expect(wrapper.text()).not.toContain('orphan')
  })
})

describe('growing a shelf', () => {
  /// The composable directly: the lane fires `nearEnd` off layout, and
  /// happy-dom has none — every scrollWidth it reports is zero.
  function driven() {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
    let api!: ReturnType<typeof useShelves>
    const wrapper = mount(
      defineComponent({
        setup() {
          api = useShelves(ref([LIBS[0]!]))
          return () => h('div')
        },
      }),
      { global: { plugins: [[VueQueryPlugin, { queryClient: client }]] } },
    )
    return { api: () => api, wrapper }
  }

  test('asks for the next page and appends it', async () => {
    const all = Array.from({ length: 25 }, (_, n) => item(`f${n}`))
    shelvesThat({ films: all })
    const { api } = driven()
    await flushPromises()
    expect(api().shelves.value[0]!.items).toHaveLength(20)

    expect(await api().more(api().shelves.value[0]!)).toBe('ok')
    await flushPromises()
    expect(api().shelves.value[0]!.items).toHaveLength(25)
  })

  test('and stops asking once the library has been read to the end', async () => {
    shelvesThat({ films: [item('f0')] })
    const { api } = driven()
    await flushPromises()
    expect(await api().more(api().shelves.value[0]!)).toBe('end')
    expect(listItems).toHaveBeenCalledTimes(1)
  })

  test('and two asks while one is out is still one request', async () => {
    // The lane asks once per width, but a resize mid-flight can ask again —
    // and two pages from the same offset append the same items twice.
    const all = Array.from({ length: 40 }, (_, n) => item(`f${n}`))
    shelvesThat({ films: all })
    const { api } = driven()
    await flushPromises()
    vi.mocked(listItems).mockClear()

    const shelf = api().shelves.value[0]!
    const [first, second] = await Promise.all([api().more(shelf), api().more(shelf)])
    await flushPromises()

    expect([first, second].sort()).toEqual(['end', 'ok'])
    expect(listItems).toHaveBeenCalledTimes(1)
    expect(api().shelves.value[0]!.items).toHaveLength(40)
  })

  test('and the screen says so, because a lane that stops growing looks finished', async () => {
    // The first page arrives, the lane asks for the second, and that one
    // fails: silence there is indistinguishable from having reached the end of
    // the library.
    const all = Array.from({ length: 100 }, (_, n) => item(`f${n}`))
    shelvesThat({ films: all, music: [] }, { pagesFail: true })
    home()
    await flushPromises()
    expect(notice.value).toBe('Could not load more from Films.')
  })

  test('a page that fails says so, rather than looking like the end', async () => {
    // A lane that stops growing is indistinguishable from one that has reached
    // the end of its library, so silence here is a lie by omission.
    const all = Array.from({ length: 25 }, (_, n) => item(`f${n}`))
    shelvesThat({ films: all })
    const { api } = driven()
    await flushPromises()
    vi.mocked(listItems).mockRejectedValue(new ApiError(500, 'no'))
    expect(await api().more(api().shelves.value[0]!)).toBe('failed')
  })

  test('and asking again drops the pages that were scrolled into', async () => {
    // They were pages of a list that failed; splicing them onto a fresh first
    // page puts somebody else's scroll position into the new answer.
    const all = Array.from({ length: 25 }, (_, n) => item(`f${n}`))
    shelvesThat({ films: all })
    const { api } = driven()
    await flushPromises()
    await api().more(api().shelves.value[0]!)
    await flushPromises()
    expect(api().shelves.value[0]!.items).toHaveLength(25)

    vi.mocked(listItems).mockRejectedValue(new ApiError(500, 'no'))
    expect(await api().retry(api().shelves.value[0]!)).toBe(false)
    await flushPromises()
    expect(api().shelves.value[0]!.items).toHaveLength(20)
  })
})

describe('what a shelf says about itself', () => {
  test('how many are showing, of how many there are', async () => {
    // "40 of 881" is why `total` is read at all. Two pages, because the lane
    // asks for the second one on mount here.
    const all = Array.from({ length: 100 }, (_, n) => item(`f${n}`))
    shelvesThat({ films: all, music: [] })
    const { wrapper } = home()
    await flushPromises()
    expect(wrapper.text()).toContain('40 of 100')
  })

  test('and its cards are the shape its media type calls for', async () => {
    // A sleeve is square and a poster is two by three; the shelf passes the
    // ratio down as a custom property and the card reads it.
    shelvesThat({ films: [item('f1')], music: [item('m1', { library_id: 'music' })] })
    const { wrapper } = home()
    await flushPromises()
    const sections = wrapper.findAll('section')
    expect(sections[0]!.attributes('style')).toContain('--card-ratio: 2 / 3')
    expect(sections[1]!.attributes('style')).toContain('--card-ratio: 1')
  })
})

describe('opening things', () => {
  test('a shelf heading goes to its library', async () => {
    shelvesThat({ films: [item('f1')], music: [] })
    const { router, wrapper } = home()
    await flushPromises()
    await wrapper
      .findAll('button')
      .find((b) => b.text().startsWith('Films'))!
      .trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/library/films')
  })

  test('a card goes to the item, under the library it was shown in', async () => {
    // The back-target has to survive a reload, and a collection can be in more
    // than one library — so the URL is the only thing that knows which. The
    // item's OWN `library_id` says music here, and the shelf it is shown in is
    // what has to win; with the two the same, either source passed.
    shelvesThat({ films: [item('f1', { library_id: 'music' })], music: [] })
    const { router, wrapper } = home()
    await flushPromises()
    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('f1'))!
      .trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/library/films/item/f1')
  })
})
