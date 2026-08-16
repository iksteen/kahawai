/// UI-17, as a standing check rather than a one-off audit.
///
/// A keyboard-only run and a screen reader are the pass; this is what stops the
/// findings coming back. Every rule here is one a mounted screen can be asked
/// about, and every one of them was broken somewhere when it was written.
///
/// What this CANNOT check, and what the pass still owes: whether an
/// announcement is intelligible, whether the focus order matches the reading
/// order, and whether anything is legible at 200% zoom. Those need a person.

import { QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { createMemoryHistory, createRouter } from 'vue-router'
import { nextTick } from 'vue'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  listLibraries: vi.fn(async () => ({ libraries: [] })),
  listItems: vi.fn(async () => ({ items: [], total: 0, limit: 100, offset: 0 })),
  itemQuery: vi.fn(),
  itemChildren: vi.fn(async () => ({ children: [] })),
  itemSetWatched: vi.fn(),
  getPrefs: vi.fn(async () => ({ prefs: [] })),
  putPref: vi.fn(),
  adminItemLog: vi.fn(),
  subtitleSearch: vi.fn(),
  subtitleDownload: vi.fn(),
  subtitleDelete: vi.fn(),
  getItemArtworkUrl: (id: string) => `/art/${id}`,
}))
vi.mock('../src/api/session.ts', () => ({
  whoAmI: () => ({ username: 'me', admin: false }),
}))
vi.mock('../src/api/capabilities.ts', () => ({
  buildProfile: () => ({}),
  loadMask: () => ({}),
  saveMask: vi.fn(),
  probedProfile: () => ({
    video: [],
    audio: [],
    containers: [],
    target_duration: { mode: 'ignore' },
  }),
}))

const api = await import('../src/api/generated/kahawai.ts')
const Home = (await import('../src/views/Home.vue')).default
const Library = (await import('../src/views/Library.vue')).default
const Detail = (await import('../src/views/Detail.vue')).default
const Settings = (await import('../src/views/Settings.vue')).default
const AppShell = (await import('../src/components/AppShell.vue')).default

const routes = [
  { path: '/', name: 'libraries', component: { template: '<div />' } },
  { path: '/library/:library', name: 'library', component: { template: '<div />' } },
  { path: '/library/:library/item/:id', name: 'detail', component: { template: '<div />' } },
  {
    path: '/library/:library/item/:id/season/:season',
    name: 'season',
    component: { template: '<div />' },
  },
  { path: '/library/:library/item/:id/play', name: 'player', component: { template: '<div />' } },
  { path: '/settings', name: 'settings', component: { template: '<div />' } },
  { path: '/admin', name: 'admin', component: { template: '<div />' } },
]

async function screen(view: unknown, at: string) {
  const router = createRouter({ history: createMemoryHistory(), routes })
  await router.push(at)
  await router.isReady()
  const wrapper = mount(view as never, {
    attachTo: document.body,
    global: {
      plugins: [
        router,
        [
          VueQueryPlugin,
          { queryClient: new QueryClient({ defaultOptions: { queries: { retry: false } } }) },
        ] as [typeof VueQueryPlugin, { queryClient: QueryClient }],
      ],
    },
  })
  await flushPromises()
  return wrapper
}

/// Everything a Tab press can land on.
const stops = (root: Element) =>
  [
    ...root.querySelectorAll<HTMLElement>(
      'a[href], button, input, select, textarea, [tabindex]:not([tabindex="-1"])',
    ),
  ].filter(
    (el) =>
      !el.hasAttribute('disabled') &&
      !el.hasAttribute('hidden') &&
      el.getAttribute('aria-hidden') !== 'true',
  )

/// What a screen reader would call this control. An icon-only button whose
/// name is the empty string is a button announced as "button".
function named(el: HTMLElement): boolean {
  if (el.getAttribute('aria-label')?.trim()) return true
  if (el.getAttribute('aria-labelledby')?.trim()) return true
  if (el.getAttribute('title')?.trim()) return true
  if ((el.textContent ?? '').trim()) return true
  // A form control's label, whether it points at the control or wraps it.
  const id = el.getAttribute('id')
  if (id && el.ownerDocument.querySelector(`label[for="${id}"]`)) return true
  return !!el.closest('label')?.textContent?.trim()
}

const film = (over: Record<string, unknown> = {}) => ({
  id: 'heat',
  kind: 'movie',
  title: 'Heat',
  year: 1995,
  played: false,
  play_count: 0,
  art_version: null,
  duration_ms: 6_000_000,
  resume_position_ms: null,
  resume_duration_ms: null,
  parent_id: null,
  show_title: null,
  season: null,
  episode: null,
  episode_end: null,
  metadata: null,
  negotiated: null,
  sources: [],
  ...over,
})

beforeEach(() => {
  vi.mocked(api.itemQuery).mockResolvedValue(film() as never)
  vi.mocked(api.listLibraries).mockResolvedValue({
    libraries: [{ id: 'films', name: 'Films', media_type: 'movies' }],
  } as never)
})
afterEach(() => vi.resetAllMocks())

describe('every control has a name', () => {
  const screens: [string, unknown, string][] = [
    ['home', Home, '/'],
    ['a library', Library, '/library/films'],
    ['an item', Detail, '/library/films/item/heat'],
    ['settings', Settings, '/settings'],
  ]

  for (const [what, view, at] of screens) {
    test(what, async () => {
      const wrapper = await screen(view, at)
      const anonymous = stops(wrapper.element as Element).filter((el) => !named(el))
      expect(anonymous.map((el) => el.outerHTML.slice(0, 120))).toEqual([])
    })
  }
})

describe('the shell', () => {
  async function shell() {
    const router = createRouter({ history: createMemoryHistory(), routes })
    await router.push('/library/films')
    await router.isReady()
    const wrapper = mount(AppShell, {
      attachTo: document.body,
      props: { libraries: [], username: 'me', admin: false },
      slots: { default: '<main><h1>A page</h1></main>' },
      global: {
        plugins: [
          router,
          [
            VueQueryPlugin,
            { queryClient: new QueryClient({ defaultOptions: { queries: { retry: false } } }) },
          ] as [typeof VueQueryPlugin, { queryClient: QueryClient }],
        ],
      },
    })
    await flushPromises()
    return wrapper
  }

  test('offers a way past the header before anything else', async () => {
    // A keyboard user landing on a library page had to walk the search box and
    // two menus before reaching the first card — on every navigation, because
    // the focus returns to the top each time.
    const wrapper = await shell()
    const first = stops(wrapper.element as Element)[0]!
    expect(first.tagName).toBe('A')
    expect(first.getAttribute('href')).toBe('#content')
  })

  test('and the target can actually hold the focus', async () => {
    // A fragment link moves the focus only to something focusable; without a
    // tabindex the browser scrolls and leaves the focus where it was, so the
    // next Tab goes straight back into the header.
    const wrapper = await shell()
    const target = wrapper.element.querySelector('#content')!
    expect(target.getAttribute('tabindex')).toBe('-1')
  })

  test('and the focus moves to the content when the screen changes', async () => {
    // A real navigation puts the focus at the top of the new document. This one
    // does not, so pressing a card left the focus on a button that no longer
    // exists: it falls to `<body>`, and the next Tab starts at the skip link
    // with nothing said about where they now are.
    const wrapper = await shell()
    const target = wrapper.element.querySelector('#content') as HTMLElement
    ;(wrapper.element.querySelector('a[href="#content"]') as HTMLElement).focus()

    const router = wrapper.vm.$router
    await router.push('/library/other')
    await flushPromises()
    await nextTick()
    expect(document.activeElement).toBe(target)
  })

  test('but not on the first render, and not for a URL that is the same screen', async () => {
    // A page that grabs the focus on load has taken it from the browser's own
    // starting point — and the player's autoplay handover changes the URL
    // without changing the screen.
    const wrapper = await shell()
    expect(document.activeElement).not.toBe(wrapper.element.querySelector('#content'))

    await wrapper.vm.$router.push('/library/films/item/heat/play')
    await flushPromises()
    await nextTick()

    // Somewhere else entirely, so "it did not move" is distinguishable from
    // "it moved to where it already was".
    const elsewhere = document.createElement('button')
    document.body.append(elsewhere)
    elsewhere.focus()
    await wrapper.vm.$router.push('/library/films/item/next/play')
    await flushPromises()
    await nextTick()
    expect(document.activeElement).toBe(elsewhere)
    elsewhere.remove()
  })

  test('and the skip link is invisible until it is focused', async () => {
    const wrapper = await shell()
    const link = wrapper.find('a[href="#content"]')
    expect(link.classes()).toContain('sr-only')
    expect(link.classes().join(' ')).toContain('focus:not-sr-only')
  })
})

describe('a heading names the screen', () => {
  const screens: [string, unknown, string][] = [
    ['home', Home, '/'],
    ['a library', Library, '/library/films'],
    ['an item', Detail, '/library/films/item/heat'],
    ['settings', Settings, '/settings'],
  ]

  for (const [what, view, at] of screens) {
    test(what, async () => {
      // Exactly one `h1`: heading navigation is how a screen reader user finds
      // where they are, and two of them is two answers to one question.
      const wrapper = await screen(view, at)
      const h1 = (wrapper.element as Element).querySelectorAll('h1')
      expect(h1).toHaveLength(1)
      expect((h1[0]!.textContent ?? '').trim()).not.toBe('')
    })
  }

  test('and the sections under it do not skip a level', async () => {
    // An `h3` under an `h1` reads as a missing section rather than a
    // sub-section of nothing.
    const wrapper = await screen(Detail, '/library/films/item/heat')
    const levels = [...(wrapper.element as Element).querySelectorAll('h1,h2,h3,h4,h5,h6')].map(
      (h) => Number(h.tagName[1]),
    )
    let deepest = 0
    for (const level of levels) {
      expect(level).toBeLessThanOrEqual(deepest + 1)
      deepest = Math.max(deepest, level)
    }
  })
})
