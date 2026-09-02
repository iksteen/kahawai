/// The library grid, mounted.
///
/// happy-dom does no layout, so the two numbers the virtualiser measures —
/// how many columns CSS resolved to, and how tall a cell is — are stubbed.
/// With them the whole thing is observable: which rows are live, what height
/// is reserved, which chunks are asked for, and what happens on a re-sort.
/// Without them it was not, and six behaviours could be deleted silently.
///
/// `laidOut()` turns the stubs on for one test; the default is the unmeasured
/// path, which is its own case — the first chunk renders plainly and the page
/// still works.

import { QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { createMemoryHistory, createRouter } from 'vue-router'

import type { ItemRowI64 } from '../src/api/generated/model/itemRowI64.ts'
import { ApiError } from '../src/api/errors.ts'
import { CHUNK, GAP } from '../src/domain/virtual.ts'
import { defineComponent, h } from 'vue'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  listItems: vi.fn(),
  listLibraries: vi.fn(),
  listArtists: vi.fn(),
  adminApplyMatch: vi.fn(),
  adminReviewSearch: vi.fn(),
  getItemArtworkUrl: (id: string) => `/api/v1/items/${id}/artwork`,
}))
const admin = { value: false }
vi.mock('../src/api/session.ts', () => ({ whoAmI: () => ({ username: 'me', admin: admin.value }) }))

const { adminApplyMatch, adminReviewSearch, listArtists, listItems, listLibraries } =
  await import('../src/api/generated/kahawai.ts')
const { clearNotices, notice } = await import('../src/composables/notices.ts')
const Library = (await import('../src/views/Library.vue')).default
const Card = (await import('../src/components/Card.vue')).default
const { DEBOUNCE_MS, useSearch } = await import('../src/composables/search.ts')

const item = (id: string, over: Record<string, unknown> = {}) =>
  ({ id, title: id, kind: 'movie', played: false, ...over }) as ItemRowI64

/// A card's row, with the fields the card actually reads.
const row = (over: Record<string, unknown>) =>
  ({
    id: 'i1',
    title: 'Heat',
    kind: 'movie',
    played: false,
    art_version: null,
    artist: null,
    year: 1995,
    season: null,
    episode: null,
    episode_end: null,
    parent_id: null,
    parent_title: null,
    resume_position_ms: null,
    resume_duration_ms: null,
    ...over,
  }) as never

function hub(total: number, over: Record<string, unknown> = {}) {
  const { failing = [] as number[], ...fields } = over as { failing?: number[] }
  vi.mocked(listItems).mockImplementation(async (params) => {
    const offset = params?.offset ?? 0
    if (failing.includes(offset / CHUNK)) throw new ApiError(503, 'the hub is restarting')
    return {
      items: Array.from({ length: Math.max(0, Math.min(CHUNK, total - offset)) }, (_, n) =>
        item(`i${offset + n}`, fields),
      ),
      total,
      limit: CHUNK,
      offset,
    }
  })
}

/// The view inside something that provides the header's search box, so the
/// filter half of this screen is reachable at all: nothing else supplies
/// `useSearchQuery`, and every test ran with an empty one.
async function filtered(text: string, at = '/library/films') {
  const router = routerFor()
  await router.push(at)
  await router.isReady()
  let box!: ReturnType<typeof useSearch>
  const wrapper = mount(
    defineComponent({
      components: { Library },
      setup() {
        box = useSearch()
        return () => h(Library)
      },
    }),
    { global: { plugins: [router, queryPlugin()] } },
  )
  box.typed(text, false)
  // Past the debounce, so the query the view reads has settled.
  await new Promise((resolve) => setTimeout(resolve, DEBOUNCE_MS + 10))
  await flushPromises()
  return { router, wrapper }
}

function routerFor() {
  return createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/', name: 'libraries', component: { template: '<div />' } },
      { path: '/library/:library', name: 'library', component: Library },
      {
        path: '/library/:library/artist/:artist',
        name: 'artist',
        component: { template: '<div />' },
      },
      { path: '/library/:library/item/:id', name: 'detail', component: { template: '<div />' } },
    ],
  })
}

const queryPlugin = () =>
  [
    VueQueryPlugin,
    { queryClient: new QueryClient({ defaultOptions: { queries: { retry: false } } }) },
  ] as [typeof VueQueryPlugin, { queryClient: QueryClient }]

async function grid(at = '/library/films') {
  const router = routerFor()
  await router.push(at)
  await router.isReady()
  const wrapper = mount(Library, { global: { plugins: [router, queryPlugin()] } })
  await flushPromises()
  return { router, wrapper }
}

/// Ten columns of 120px, cells 186px tall, a 3000px window. Restored by
/// `afterEach` through `vi.unstubAllGlobals` and the saved prototype method.
const realRect = Element.prototype.getBoundingClientRect
function laidOut({ cols = 10, cellH = 186, viewport = 3000 } = {}) {
  vi.stubGlobal('getComputedStyle', () => ({
    gridTemplateColumns: Array<string>(cols).fill('120px').join(' '),
  }))
  vi.stubGlobal('innerHeight', viewport)
  Element.prototype.getBoundingClientRect = function rect(this: Element) {
    // A cell is one card tall. The wrapper sits at the top of the document, so
    // its VIEWPORT top is minus however far the page is scrolled — which is
    // what makes `top + scrollY` the constant the virtualiser reads it as.
    return {
      top: this.tagName === 'LI' ? 0 : -window.scrollY,
      height: this.tagName === 'LI' ? cellH : 0,
    } as DOMRect
  }
  return { cols, rowH: cellH + GAP }
}

beforeEach(() => {
  hub(250)
  vi.mocked(listLibraries).mockResolvedValue({
    libraries: [
      { id: 'films', name: 'Films', media_type: 'movies' },
      { id: 'music', name: 'Music', media_type: 'music' },
    ],
  })
  vi.mocked(listArtists).mockResolvedValue({
    artists: [
      { key: 'bjork', name: 'Björk', album_count: 12 },
      { key: 'various artists', name: 'Various Artists', album_count: 4 },
    ],
    total: 2,
    limit: 100,
    offset: 0,
  })
  clearNotices()
})
afterEach(() => {
  vi.resetAllMocks()
  vi.unstubAllGlobals()
  Element.prototype.getBoundingClientRect = realRect
  Object.defineProperty(window, 'scrollY', { value: 0, configurable: true })
})

describe('opening a library', () => {
  test('names it, and says how many it holds', async () => {
    const { wrapper } = await grid()
    expect(wrapper.find('h1').text()).toBe('Films')
    expect(wrapper.text()).toContain('250')
  })

  test('draws the first chunk', async () => {
    const { wrapper } = await grid()
    expect(wrapper.findAll('li')).toHaveLength(CHUNK)
    expect(wrapper.text()).toContain('i0')
  })

  test('offers the way home, because the wordmark is a menu now', async () => {
    const { router, wrapper } = await grid()
    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('Home'))!
      .trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/')
  })

  test('a card opens its item, under this library', async () => {
    const { router, wrapper } = await grid()
    await wrapper
      .findAll('button')
      .find((b) => b.text().includes('i0'))!
      .trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/library/films/item/i0')
  })

  test('does not turn artist API paging into a load-more interaction', async () => {
    vi.mocked(listArtists).mockResolvedValue({
      artists: [{ key: 'bjork', name: 'Björk', album_count: 12 }],
      total: 101,
      limit: 100,
      offset: 0,
    })
    const { wrapper } = await grid('/library/music')
    expect(wrapper.text()).not.toContain('More artists')
  })

  test('does not render an empty Artists section for item-only search results', async () => {
    vi.mocked(listArtists).mockResolvedValue({
      artists: [],
      total: 0,
      limit: 100,
      offset: 0,
    })
    const { wrapper } = await filtered('heat', '/library/music')
    expect(wrapper.findAll('h2').map((heading) => heading.text())).toEqual(['Albums and songs'])
  })
})

describe('when something will not load', () => {
  test('an unknown library reaches the canonical item-route refusal', async () => {
    vi.mocked(listItems).mockRejectedValue(new ApiError(404, 'library not found'))
    const { wrapper } = await grid('/library/gone')

    expect(listItems).toHaveBeenCalledWith(expect.objectContaining({ library: 'gone' }))
    expect(wrapper.text()).toContain('library not found')
  })

  test('the grid says so and offers to ask again', async () => {
    hub(250, { failing: [0] })
    const { wrapper } = await grid()
    expect(wrapper.text()).toContain('restarting')

    hub(250)
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Try again')!
      .trigger('click')
    await flushPromises()
    expect(wrapper.text()).not.toContain('restarting')
    expect(wrapper.text()).toContain('i0')
  })

  test('and the chrome stays up while it does', async () => {
    // Every early return swaps the whole tree, and swapping the tree destroys
    // the search box somebody is typing into.
    hub(250, { failing: [0] })
    const { wrapper } = await grid()
    expect(wrapper.find('h1').exists()).toBe(true)
    expect(wrapper.find('select').exists()).toBe(true)
  })

  test('the library details failing is a notice, not the screen', async () => {
    // This request failing alone leaves a perfectly good grid underneath it.
    vi.mocked(listLibraries).mockRejectedValue(new ApiError(500, 'nope'))
    const { wrapper } = await grid()
    expect(wrapper.text()).toContain('i0')
    expect(notice.value).toContain('library details')
  })
})

describe('an empty library', () => {
  test('says what to do about it', async () => {
    hub(0)
    const { wrapper } = await grid()
    expect(wrapper.text()).toContain('Attach a collection')
  })
})

describe('sorting', () => {
  test('asks the hub again, in the new order', async () => {
    const { wrapper } = await grid()
    vi.mocked(listItems).mockClear()
    await wrapper.find('select').setValue('-added')
    await flushPromises()
    expect(listItems).toHaveBeenCalledWith(expect.objectContaining({ sort: '-added', offset: 0 }))
  })
})

/// Not reachable through the grid in a test environment: without a
/// measurement the view renders the first chunk plainly, and the first chunk
/// is either loaded or the whole page is an error. The card is where the rule
/// lives, so the card is where it is checked.
describe('a cell whose row has not arrived', () => {
  test('is the same box as one that has, structurally', async () => {
    // It must occupy exactly what it will occupy once it arrives, or the grid
    // resizes as chunks land — which is the layout shift the reserved height
    // exists to avoid — and the row height stops being a constant the
    // measurement can trust.
    //
    // Counting children was not enough: three empty spans are also three
    // children, and every mutation that made the ghost a different HEIGHT
    // passed. These are the three things that give it its height.
    const pending = mount(Card, { props: {} })
    const arrived = mount(Card, { props: { item: row({}) } })

    // The art keeps the shape the real one takes, read off the same property.
    // Only the class: happy-dom drops an `aspect-ratio` whose value is a
    // `var()`, so the rule itself cannot be read here. The clamp below is in
    // the class list because that one did not have to be.
    expect(pending.find('.ghost-art').exists()).toBe(true)
    // Two lines of title either way, whatever is in them.
    for (const card of [pending, arrived]) {
      expect(card.find('.card-title').classes()).toContain('line-clamp-2')
      expect(card.find('.card-title').classes()).toContain('h-[2.7em]')
    }
    // Non-breaking spaces, so each line still takes a line box. `.text()`
    // trims, and a trimmed nbsp is empty — the markup is what matters here.
    expect(pending.find('.card-title').html()).toContain('&nbsp;')
    expect(pending.find('.card-meta').html()).toContain('&nbsp;')
  })

  test('and the real card is never shorter than it', () => {
    // `metaLine` is '' for a film with no year, and an empty span takes no
    // line box: one short cell makes its whole grid row short, and the
    // measurement is taken off one cell.
    const yearless = mount(Card, { props: { item: row({ year: null }) } })
    expect(yearless.find('.card-meta').text()).toBe('—')
  })

  test('and says how many files there are, when there is more than one', () => {
    expect(
      mount(Card, { props: { item: row({ sources: 3 }) } })
        .find('.card-meta')
        .text(),
    ).toBe('1995 · 3 sources')
    expect(
      mount(Card, { props: { item: row({ sources: 1 }) } })
        .find('.card-meta')
        .text(),
    ).toBe('1995')
  })

  test('and its state is in the name, not only in a badge', () => {
    // `title` on a span inside a button contributes nothing to the button's
    // accessible name, so the card announced the same whether it was
    // unwatched, half-watched or finished.
    expect(mount(Card, { props: { item: row({ played: true }) } }).text()).toContain('seen')
    expect(
      mount(Card, {
        props: { item: row({ resume_position_ms: 60, resume_duration_ms: 120 }) },
      }).text(),
    ).toContain('part-watched')
    expect(mount(Card, { props: { item: row({}) } }).text()).not.toContain('watched')
  })

  test('and says nothing to a screen reader, because there is nothing to say', () => {
    const pending = mount(Card, { props: {} })
    expect(pending.attributes('aria-hidden')).toBe('true')
    // Not a button: there is nothing to press yet.
    expect(pending.find('button').exists()).toBe(false)
  })
})

describe('once the page has been measured', () => {
  test('only the rows on screen exist, and the rest is reserved height', async () => {
    // That is the difference from infinite scroll, where the page grows as
    // you go and the scrollbar jumps under the thumb every time it does.
    const { cols, rowH } = laidOut()
    hub(2242)
    const { wrapper } = await grid()
    await flushPromises()

    const rows = Math.ceil(2242 / cols)
    expect(wrapper.find('div.relative').attributes('style')).toContain(
      `height: ${rows * rowH - GAP}px`,
    )
    // Nowhere near 2242 cells in the document.
    expect(wrapper.findAll('li').length).toBeLessThan(300)
    expect(wrapper.findAll('li').length).toBeGreaterThan(cols)
  })

  test('and it asks for the chunks those rows need', async () => {
    laidOut()
    hub(2242)
    await grid()
    await flushPromises()
    const asked = vi.mocked(listItems).mock.calls.map((c) => c[0]?.offset)
    expect(asked).toContain(0)
    expect(asked).toContain(100)
    // Not the whole library.
    expect(asked).not.toContain(2200)
  })

  test('re-sorting refills the grid rather than leaving one row of cards', async () => {
    // Scrolling to the top when already there fires no scroll event, and a
    // re-sort changes neither the total nor the metric — so every path that
    // would have recomputed was watching something that had not moved, and
    // the grid sat holding ten cards inside a 19986px container.
    const { cols } = laidOut()
    hub(2242)
    const { wrapper } = await grid()
    await flushPromises()
    const before = wrapper.findAll('li').length
    expect(before).toBeGreaterThan(cols)

    await wrapper.find('select').setValue('-added')
    await flushPromises()
    expect(wrapper.findAll('li').length).toBe(before)
  })

  test('and a re-sort re-fetches every chunk it is showing', async () => {
    // `loaded` deliberately still holds the previous result set, so a guard
    // reading it skipped chunks the new one had never fetched — leaving
    // ninety cells as permanent placeholders once the first chunk landed.
    laidOut()
    hub(2242)
    const { wrapper } = await grid()
    await flushPromises()
    vi.mocked(listItems).mockClear()

    await wrapper.find('select').setValue('-added')
    await flushPromises()
    const asked = vi.mocked(listItems).mock.calls.map((c) => c[0]?.offset)
    expect(asked).toContain(0)
    expect(asked).toContain(100)
  })

  test('a music library starts at Album Artists instead of the album grid', async () => {
    const { wrapper, router } = await grid('/library/music')
    await flushPromises()
    expect(wrapper.text()).toContain('Björk')
    expect(wrapper.text()).toContain('12 albums')
    expect(listItems).not.toHaveBeenCalled()
    await wrapper.findAll('.artist-tile')[0]!.trigger('click')
    await flushPromises()
    expect(router.currentRoute.value.name).toBe('artist')
    expect(router.currentRoute.value.params.artist).toBe('bjork')
  })
})

describe('filtering it from the header', () => {
  test('the count says what it is filtering from', async () => {
    // 12 on its own leaves you wondering whether the other 2230 are missing
    // or excluded.
    vi.mocked(listItems).mockImplementation(async (params) => ({
      items: params?.q ? [item('i0')] : Array.from({ length: 100 }, (_, n) => item(`i${n}`)),
      total: params?.q ? 12 : 250,
      limit: CHUNK,
      offset: params?.offset ?? 0,
    }))
    const { wrapper } = await filtered('heat')
    expect(wrapper.text()).toContain('12/250')
  })

  test('the filter reaches the hub', async () => {
    const { wrapper } = await filtered('heat')
    expect(listItems).toHaveBeenCalledWith(expect.objectContaining({ q: 'heat' }))
    expect(wrapper.text()).toContain('Films')
  })

  test('and nothing matching says so, quoting it', async () => {
    hub(0)
    const { wrapper } = await filtered('zzz')
    expect(wrapper.text()).toContain('Nothing matches “zzz”')
    expect(wrapper.text()).not.toContain('Attach a collection')
  })

  test('and the heading drops it, from the keyboard as well as the mouse', async () => {
    // The heading is where somebody looks when the page says twelve of two
    // thousand — the other half of the ✕ in the box. A heading with a click
    // handler is unreachable without a pointer, so it is a button when it is
    // pressable.
    const { wrapper } = await filtered('heat')
    vi.mocked(listItems).mockClear()

    const heading = wrapper.find('h1 button')
    expect(heading.exists()).toBe(true)
    await heading.trigger('click')
    await new Promise((resolve) => setTimeout(resolve, DEBOUNCE_MS + 10))
    await flushPromises()
    expect(listItems).toHaveBeenCalledWith(expect.objectContaining({ q: '' }))
  })

  test('and with nothing to drop it is not a control at all', async () => {
    // A heading that looks pressable and does nothing is worse than one that
    // does not.
    const { wrapper } = await grid()
    expect(wrapper.find('h1 button').exists()).toBe(false)
  })
})

describe('scrolling it', () => {
  test('extends the live rows and asks for what they need', async () => {
    // Nothing else drives this: the scroll listener is the only path from a
    // wheel to a fetch, and deleting it changed no test.
    laidOut({ viewport: 900 })
    hub(2242)
    const { wrapper } = await grid()
    await flushPromises()
    const before = wrapper.findAll('li').length
    vi.mocked(listItems).mockClear()

    Object.defineProperty(window, 'scrollY', { value: 4000, configurable: true })
    window.dispatchEvent(new Event('scroll'))
    await flushPromises()

    // Different rows, and the chunk they live in. Slightly MORE of them than
    // at the top, where the overscan above the first row is clipped away.
    expect(wrapper.findAll('li')[0]!.attributes('aria-posinset')).not.toBe('1')
    expect(wrapper.findAll('li').length).toBeGreaterThanOrEqual(before)
    expect(vi.mocked(listItems).mock.calls.map((c) => c[0]?.offset)).toContain(200)
  })
})

describe('hand-matching from the grid (HUB-8)', () => {
  beforeEach(() => {
    admin.value = true
    vi.mocked(adminReviewSearch).mockResolvedValue({ candidates: [] } as never)
    vi.mocked(adminApplyMatch).mockResolvedValue({ ok: true } as never)
  })
  afterEach(() => (admin.value = false))

  test('is not offered to somebody who is not an administrator', async () => {
    admin.value = false
    const { wrapper } = await grid()
    expect(wrapper.findAll('[aria-label*="match"]')).toHaveLength(0)
  })

  test('nor on an episode, which has no identity of its own', async () => {
    // An episode inherits its show's match; the show is where you would fix it.
    hub(1, { kind: 'episode' })
    const { wrapper } = await grid()
    expect(wrapper.findAll('[aria-label*="match"]')).toHaveLength(0)
  })

  test('and opens a dialog anchored on the file', async () => {
    hub(1, { file_title: 'Heat', file_year: 1995 })
    const { wrapper } = await grid()
    await wrapper.find('[aria-label*="match"]').trigger('click')
    await flushPromises()
    expect(wrapper.find('[role="dialog"]').exists()).toBe(true)
    expect(adminReviewSearch).toHaveBeenCalledWith(
      expect.objectContaining({ query: 'Heat', year: 1995 }),
    )
  })

  test('closing it without applying re-reads nothing', async () => {
    hub(1)
    const { wrapper } = await grid()
    await wrapper.find('[aria-label*="match"]').trigger('click')
    await flushPromises()
    const reads = vi.mocked(listItems).mock.calls.length

    await wrapper.find('[role="dialog"] [aria-label="Close"]').trigger('click')
    await flushPromises()
    expect(wrapper.find('[role="dialog"]').exists()).toBe(false)
    expect(vi.mocked(listItems).mock.calls.length).toBe(reads)
  })

  test('and applying one re-reads that row’s chunk', async () => {
    // A match rewrites the title, the year and the artwork of exactly one row.
    // Re-reading the library would throw away every chunk that had been
    // scrolled through.
    hub(1, { match_confidence: 'weak' })
    vi.mocked(adminReviewSearch).mockResolvedValue({ candidates: [] } as never)
    const { wrapper } = await grid()
    await wrapper.find('[aria-label*="match"]').trigger('click')
    await flushPromises()
    const reads = vi.mocked(listItems).mock.calls.length

    await wrapper
      .findAll('[role="dialog"] button')
      .find((b) => b.text() === 'Confirm current')!
      .trigger('click')
    await flushPromises()
    expect(adminApplyMatch).toHaveBeenCalledWith(
      'i0',
      expect.objectContaining({ action: 'confirm' }),
    )
    expect(vi.mocked(listItems).mock.calls.length).toBe(reads + 1)
  })
})
