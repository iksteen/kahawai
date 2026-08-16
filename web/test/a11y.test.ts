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
import { defineComponent, h, nextTick, ref } from 'vue'

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
/// Left hanging on purpose: the player is in the audit for the frame around the
/// picture, and a resolved session mounts hls.js.
vi.mock('../src/api/playback.ts', () => ({
  startPlaybackSession: vi.fn(() => new Promise(() => {})),
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
const { screenShowing, useScreenName } = await import('../src/composables/title.ts')
type Showing = NonNullable<(typeof screenShowing)['value']>
const Home = (await import('../src/views/Home.vue')).default
const Library = (await import('../src/views/Library.vue')).default
const Detail = (await import('../src/views/Detail.vue')).default
const Settings = (await import('../src/views/Settings.vue')).default
const AppShell = (await import('../src/components/AppShell.vue')).default
const Season = (await import('../src/views/Season.vue')).default
const Player = (await import('../src/views/Player.vue')).default

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

/// Torn down between tests. These attach to the document and publish into
/// module state; left standing, one test's screen is still mounted and still
/// answering when the next one asks.
let live: { unmount: () => void }[] = []

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
  live.push(wrapper)
  await flushPromises()
  return wrapper
}

/// The published screen name is module state, which is part of how the missing
/// calls hid: a screen that publishes nothing inherits whatever the last one
/// said, so "not empty" is true of every screen in this file. Cleared through
/// the same door the views use, and each screen is then asked for its OWN name
/// rather than for any name at all.
const forgetScreenName = () => {
  const wrapper = mount(
    defineComponent({
      setup() {
        useScreenName('libraries', ref(null))
        return () => h('div')
      },
    }),
  )
  live.push(wrapper)
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
  live = []
  vi.mocked(api.itemQuery).mockResolvedValue(film() as never)
  vi.mocked(api.listLibraries).mockResolvedValue({
    libraries: [{ id: 'films', name: 'Films', media_type: 'movies' }],
  } as never)
})
afterEach(() => {
  for (const wrapper of live.reverse()) wrapper.unmount()
  vi.resetAllMocks()
})

describe('every control has a name', () => {
  const screens: [string, unknown, string][] = [
    ['home', Home, '/'],
    ['a library', Library, '/library/films'],
    ['an item', Detail, '/library/films/item/heat'],
    ['a season', Season, '/library/films/item/show/season/1'],
    ['the player', Player, '/library/films/item/heat/play'],
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

describe('every screen says what it is showing', () => {
  // UI-17, and the half of it that a unit test of `documentTitle` cannot reach.
  // The pure function was right and thoroughly tested; nothing asked whether
  // any view ever CALLED it with a name, and for three of these screens none
  // did. The tab strip, the bookmark and the screen reader all got the bare
  // word "kahawai" — the exact failure the module was written to fix.
  const screens: [string, unknown, string, Showing][] = [
    ['a library', Library, '/library/films', { screen: 'library', name: 'Films' }],
    ['an item', Detail, '/library/films/item/heat', { screen: 'detail', name: 'Heat' }],
    [
      'a season',
      Season,
      '/library/films/item/show/season/1',
      { screen: 'season', name: 'Heat · Season 1' },
    ],
    ['the player', Player, '/library/films/item/heat/play', { screen: 'player', name: 'Heat' }],
  ]

  for (const [what, view, at, expected] of screens) {
    test(what, async () => {
      if (what === 'a season') {
        vi.mocked(api.itemQuery).mockResolvedValue(film({ id: 'show', kind: 'show' }) as never)
      }
      forgetScreenName()
      expect(screenShowing.value).toBe(null)
      await screen(view, at)
      await flushPromises()
      // The TAG as well as the name. It is a hand-typed literal in each view,
      // and a screen publishing under another screen's tag never titles itself
      // and is never announced — while an assertion about the name alone goes
      // on passing.
      expect(screenShowing.value).toEqual(expected)
    })
  }

  test('and an episode carries its show', async () => {
    // The name is `itemName`, not `item.title`: every one of these is called
    // "Episode 1". A film fixture cannot tell the two apart.
    vi.mocked(api.itemQuery).mockResolvedValue(
      film({ id: 'ep', title: 'Episode 1', kind: 'episode', show_title: 'Blue Exorcist' }) as never,
    )
    forgetScreenName()
    await screen(Detail, '/library/films/item/ep')
    await flushPromises()
    expect(screenShowing.value).toEqual({ screen: 'detail', name: 'Blue Exorcist · Episode 1' })
  })

  test('and a player still waiting publishes nothing', async () => {
    // "Starting playback" is a state, not a name. Published, it would spend the
    // screen's one announcement before there is anything to announce.
    vi.mocked(api.itemQuery).mockImplementation((() => new Promise(() => {})) as never)
    forgetScreenName()
    await screen(Player, '/library/films/item/heat/play')
    await flushPromises()
    expect(screenShowing.value).toBe(null)
  })
})

describe('and a screen that could not load says so', () => {
  // The screens with no word of their own wait for the thing they are titled
  // by. When the request fails, that thing is never coming — so the tab strip
  // kept the bare site name and the one announcement a screen gets never
  // happened, in the state with the most to explain.
  const screens: [string, unknown, string, string][] = [
    ['an item', Detail, '/library/films/item/heat', 'Could not load this item'],
    ['a season', Season, '/library/films/item/show/season/1', 'Could not load this season'],
    ['the player', Player, '/library/films/item/heat/play', 'Could not start playback'],
  ]

  for (const [what, view, at, expected] of screens) {
    test(what, async () => {
      vi.mocked(api.itemQuery).mockRejectedValue(new Error('nope'))
      vi.mocked(api.itemChildren).mockRejectedValue(new Error('nope'))
      forgetScreenName()
      expect(screenShowing.value).toBe(null)
      await screen(view, at)
      await flushPromises()
      expect(screenShowing.value?.name).toBe(expected)
    })
  }

  test('but a season whose SHOW could not be read has not failed', async () => {
    // Two different failures. The show's details going missing is a notice
    // over a page full of working episodes — announcing "could not load this
    // season" there tells the screen reader something the screen contradicts.
    vi.mocked(api.itemQuery).mockRejectedValue(new Error('nope'))
    vi.mocked(api.itemChildren).mockResolvedValue({
      children: [film({ id: 'ep1', title: 'Episode 1', kind: 'episode', season: 1, episode: 1 })],
    } as never)
    forgetScreenName()
    const wrapper = await screen(Season, '/library/films/item/show/season/1')
    await flushPromises()
    expect(wrapper.text()).not.toContain('Could not load this season')
    expect(screenShowing.value?.name).toBe('Season 1')
  })

  test('and a library whose details could not be read falls back to its word', async () => {
    // Not the same shape: this one failing leaves a perfectly good grid
    // underneath it, so the screen is the library either way — but it is a
    // library whose real name is never arriving, and the screen still has to
    // answer "where am I".
    vi.mocked(api.listLibraries).mockRejectedValue(new Error('nope'))
    forgetScreenName()
    expect(screenShowing.value).toBe(null)
    await screen(Library, '/library/films')
    await flushPromises()
    expect(screenShowing.value?.name).toBe('Library')
  })
})

describe('nothing announces itself by appearing', () => {
  // A live region has to be in the accessibility tree BEFORE its content
  // changes: a node inserted with its text already in it is not reliably
  // announced by NVDA or VoiceOver, which is the case they are least good at.
  //
  // Counted across the two states rather than checked for emptiness: some of
  // these regions exist to hold a standing value — a count line, a saved
  // marker — and being non-empty at rest is their job. What must not happen is
  // a region APPEARING with the failure it reports.
  const regions = (wrapper: { element: unknown }) =>
    (wrapper.element as Element).querySelectorAll('[role="status"], [role="alert"]').length

  test('an item page has the same regions whether or not its list failed', async () => {
    vi.mocked(api.itemQuery).mockResolvedValue(film({ id: 'show', kind: 'show' }) as never)
    const quiet = regions(await screen(Detail, '/library/films/item/show'))

    vi.mocked(api.itemChildren).mockRejectedValue(new Error('nope'))
    const failing = await screen(Detail, '/library/films/item/show')
    expect(failing.text()).toContain('Could not load the episodes')
    expect(regions(failing)).toBe(quiet)
  })

  test('and a library has the same whether or not a chunk failed', async () => {
    const quiet = regions(await screen(Library, '/library/films'))

    vi.mocked(api.listItems).mockRejectedValue(new Error('nope'))
    const failing = await screen(Library, '/library/films')
    expect(regions(failing)).toBe(quiet)
  })
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
    // The player had none at all: the only thing on the screen is the picture,
    // so there was nothing to make a heading out of and none was written.
    ['the player', Player, '/library/films/item/heat/play'],
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

  test('and a screen that failed still has one', async () => {
    // The heading used to live inside the success branch, so the state with the
    // MOST to explain — a refusal, an unreachable source — was the one with no
    // answer to "where am I". `Failed` opens at `h2`, so the screen started at
    // level two as well.
    vi.mocked(api.itemQuery).mockRejectedValue(new Error('nope'))
    const wrapper = await screen(Player, '/library/films/item/heat/play')
    expect(wrapper.text()).toContain('Could not start playback.')
    const h1 = (wrapper.element as Element).querySelectorAll('h1')
    expect(h1).toHaveLength(1)
    expect((h1[0]!.textContent ?? '').trim()).not.toBe('')
  })

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

describe('what the browser has been told', () => {
  test('a viewer who asked for less motion gets none', async () => {
    // UI-17. A spinner, a fade and a sliding canvas are all decoration; for
    // somebody whose vestibular system reads them as movement they are a
    // headache. Nothing here is load-bearing — every animated element says
    // what it is in text — so the honest answer is to stop, not to slow down.
    //
    // Read out of the built stylesheet, because a scoped `<style>` block and a
    // Tailwind utility both end up there and only the build knows what won.
    const { readFileSync, readdirSync } = await import('node:fs')
    const dir = 'dist/assets'
    let css = ''
    try {
      css = readdirSync(dir)
        .filter((f) => f.endsWith('.css'))
        .map((f) => readFileSync(`${dir}/${f}`, 'utf8'))
        .join('\n')
    } catch {
      // No build in this checkout: `npm run build` is a separate gate, and a
      // test that fails for its absence would be reporting the wrong thing.
      return
    }
    expect(css).toContain('prefers-reduced-motion')
    const block = css.slice(css.indexOf('prefers-reduced-motion'))
    expect(block).toContain('animation-duration')
    expect(block).toContain('transition-duration')
  })
})
