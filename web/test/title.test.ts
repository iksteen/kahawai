/// UI-17: what the document is called, and the one thing that tells a screen
/// reader the screen changed.
///
/// A single-page app does not reload, so the browser's own announcement never
/// happens. Every rule here is about NOT saying too much: a title change alone
/// is announced by some readers and not others, so a live region carries the
/// same words — and a region that repeats itself is worse than one that says
/// nothing.

import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { defineComponent, h, ref } from 'vue'
import { createMemoryHistory, createRouter, type Router } from 'vue-router'

const Blank = { template: '<div />' }

import { documentTitle, itemName, type Screen } from '../src/domain/titles.ts'
import {
  screenName,
  screenShowing,
  useDocumentTitle,
  useScreenName,
} from '../src/composables/title.ts'

describe('what a screen is called', () => {
  test('the thing on it, then the site', () => {
    // A tab strip truncates from the right, and the site is the half you
    // already know.
    expect(documentTitle('library', 'Films')).toBe('Films · kahawai')
    expect(documentTitle('detail', 'Heat')).toBe('Heat · kahawai')
  })

  test('and the screen’s own word until the thing arrives', () => {
    // The name is a round trip behind the route.
    expect(documentTitle('library', null)).toBe('Library · kahawai')
    expect(documentTitle('settings', null)).toBe('Settings · kahawai')
    expect(documentTitle('libraries', undefined)).toBe('Home · kahawai')
  })

  test('and nothing over-specific for a screen that names itself', () => {
    // An item page's heading IS the title; "Item · kahawai" would be a worse
    // answer than the site alone.
    expect(documentTitle('detail', null)).toBe('kahawai')
    expect(documentTitle('player', '')).toBe('kahawai')
  })

  test('and a name of nothing but spaces is not a name', () => {
    expect(documentTitle('library', '   ')).toBe('Library · kahawai')
  })

  test('and the gate is a screen too', () => {
    // The router still names whichever page the last session ended on, so a
    // sign-in form used to be titled "Home".
    expect(documentTitle('login', null)).toBe('Sign in · kahawai')
    expect(documentTitle('setup', null)).toBe('Set up · kahawai')
    expect(documentTitle('failed', null)).toBe('Unavailable · kahawai')
  })

  test('and booting is not a screen, on purpose', () => {
    // It is over in about forty milliseconds and shows a deliberately blank
    // page. Retitling the tab for that long is the flicker the blank page
    // exists to avoid, and `index.html` already says this word.
    expect(documentTitle('boot', null)).toBe('kahawai')
  })
})

describe('what to call the item on the screen', () => {
  test('a film is its own title', () => {
    expect(itemName({ title: 'Heat', show_title: null })).toBe('Heat')
  })

  test('an episode carries the show', () => {
    // "Episode 1" is what a great many of these are called, and a tab strip
    // full of them names nothing.
    expect(itemName({ title: 'Episode 1', show_title: 'Blue Exorcist' })).toBe(
      'Blue Exorcist · Episode 1',
    )
  })

  test('and a missing half is not punctuation', () => {
    expect(itemName({ title: '', show_title: 'Blue Exorcist' })).toBe('Blue Exorcist')
    expect(itemName({ title: 'Heat', show_title: '  ' })).toBe('Heat')
  })
})

/// The app root, which knows which screen is up and nothing about what is on
/// it. Mounted separately from the screens, because that separation is the
/// thing that goes wrong.
function driven(route: Screen, address: string = route) {
  const at = ref(route)
  /// What the root passes as an arrival: the error boundary's key, which is
  /// the address for every screen but the player. Defaulted to the screen, so
  /// a test that only changes screens gets an arrival with each one.
  const went = ref<string>(address)
  const wrapper = mount(
    defineComponent({
      setup() {
        useDocumentTitle(at, went)
        return () => h('div')
      },
    }),
  )
  mounted.push(wrapper)
  return { at, went, wrapper }
}

/// A screen publishing its own name, the way a view does. Separate from
/// `driven` so a test can land the name late, or take the screen away, or —
/// the case that matters — leave the old one standing across a route change.
///
/// It reads the address off a real router, because that is where the tag comes
/// from: a publisher standing at `/item/heat` cannot name `/item/sleepers`,
/// which is the whole guarantee.
async function publisher(at: string, name: string | null = null) {
  const source = ref<string | null>(name)
  const router: Router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/library/:library', name: 'library', component: Blank },
      { path: '/library/:library/item/:id', name: 'detail', component: Blank },
      { path: '/library/:library/item/:id/play', name: 'player', component: Blank },
      { path: '/:rest(.*)', name: 'libraries', component: Blank },
    ],
  })
  await router.push(at)
  await router.isReady()
  const wrapper = mount(
    defineComponent({
      setup() {
        useScreenName(source)
        return () => h('div')
      },
    }),
    { global: { plugins: [router] } },
  )
  mounted.push(wrapper)
  return { source, wrapper, router }
}

/// Unmounted between tests. `screenShowing` is module state, so a publisher
/// left standing answers the next test's first assertion for it.
let mounted: { unmount: () => void }[] = []

beforeEach(() => {
  vi.useFakeTimers()
  document.title = 'kahawai'
  mounted = []
})
afterEach(() => {
  for (const wrapper of mounted.reverse()) wrapper.unmount()
  vi.useRealTimers()
})

/// Real addresses, because the publication is tagged with one and the root is
/// armed by one — a test that invents a key for each side proves only that the
/// two strings it wrote are equal.
const HOME = '/'
const LIB = '/library/films'
const ITEM = '/library/films/item/heat'
const OTHER = '/library/films/item/sleepers'
/// The player's key deliberately drops the item: an autoplay handover changes
/// the URL and must not remount the frame. See `boundaryKey`.
const PLAY = '/library/films/item/heat/play'
const PLAYING = 'player:films'

describe('moving between screens', () => {
  test('sets the document title at once', async () => {
    const { at, went } = driven('libraries')
    expect(document.title).toBe('Home · kahawai')
    at.value = 'settings'
    went.value = 'settings'
    await flushPromises()
    expect(document.title).toBe('Settings · kahawai')
  })

  test('and says the same words out loud', async () => {
    // A title change alone is announced by some screen readers and not others.
    const { at, went } = driven('libraries')
    at.value = 'settings'
    went.value = 'settings'
    await flushPromises()
    expect(screenName.value).toBe('Settings · kahawai')
  })

  test('and stops saying them, so the next visit is announced too', async () => {
    // A live region only speaks when its content CHANGES: left standing, going
    // back to a screen you have already been on says nothing at all.
    const { at, went } = driven('libraries')
    at.value = 'settings'
    went.value = 'settings'
    await flushPromises()
    vi.advanceTimersByTime(1500)
    expect(screenName.value).toBe('')

    at.value = 'libraries'

    went.value = 'libraries'
    await flushPromises()
    at.value = 'settings'
    went.value = 'settings'
    await flushPromises()
    expect(screenName.value).toBe('Settings · kahawai')
  })

  test('and the title follows the name arriving late', async () => {
    // The name is a round trip behind the route. A screen with a placeholder
    // shows it meanwhile — a tab strip cannot wait — but says nothing:
    // "Library" is the word the announcement exists to replace, and a screen
    // is only announced once, so saying it spends the answer to "where am I"
    // on the question.
    const { at, went } = driven('libraries', HOME)
    vi.advanceTimersByTime(1500)
    at.value = 'library'
    went.value = LIB
    await flushPromises()
    expect(document.title).toBe('Library · kahawai')
    expect(screenName.value).toBe('')

    await publisher(LIB, 'Films')
    await flushPromises()
    expect(document.title).toBe('Films · kahawai')
    expect(screenName.value).toBe('Films · kahawai')
  })

  test('and a screen titled by its contents waits for them', async () => {
    // The item's title is a round trip behind the route, and this screen has no
    // word of its own to fall back on. Announcing on arrival announced the
    // literal word "kahawai" — and then the real title landed under the
    // once-per-screen guard and was never said at all.
    const { at, went } = driven('libraries', HOME)
    vi.advanceTimersByTime(1500)
    at.value = 'detail'
    went.value = ITEM
    await flushPromises()
    expect(document.title).toBe('kahawai')
    expect(screenName.value).toBe('')

    await publisher(ITEM, 'Heat')
    await flushPromises()
    expect(document.title).toBe('Heat · kahawai')
    expect(screenName.value).toBe('Heat · kahawai')
  })

  test('and still says it only once', async () => {
    const { at, went } = driven('libraries', HOME)
    at.value = 'detail'
    went.value = ITEM
    await flushPromises()
    const item = await publisher(ITEM, 'Heat')
    await flushPromises()
    vi.advanceTimersByTime(1500)
    expect(screenName.value).toBe('')

    // A retitle on the SAME screen — the hub answering again with a corrected
    // name — is not an arrival.
    item.source.value = 'Heat (1995)'
    await flushPromises()
    expect(document.title).toBe('Heat (1995) · kahawai')
    expect(screenName.value).toBe('')
  })

  test('and the screen you are LEAVING cannot name the one you arrive on', async () => {
    // The route changes before the outgoing view is torn down: a `pre`-flush
    // watcher runs ahead of the component update that unmounts it. Untagged,
    // the new screen was paired with the old screen's name — arriving on an
    // item announced "Films", and the item's own title, landing a beat later,
    // was swallowed as a repeat of a screen already announced.
    const { at, went } = driven('library', LIB)
    const library = await publisher(LIB, 'Films')
    await flushPromises()
    vi.advanceTimersByTime(1500)

    at.value = 'detail'
    went.value = ITEM
    await flushPromises()
    expect(document.title).toBe('kahawai')
    expect(screenName.value).toBe('')

    // Only now does the outgoing screen go, which is the order Vue uses.
    library.wrapper.unmount()
    await publisher(ITEM, 'Heat')
    await flushPromises()
    expect(document.title).toBe('Heat · kahawai')
    expect(screenName.value).toBe('Heat · kahawai')
  })

  test('and the next screen is announced even with the SAME words', async () => {
    // Pressing Play on an item page: two screens, one name. A live region only
    // speaks when its content changes, so with the item page's sentence still
    // standing, setting the identical string said nothing — and the everyday
    // path is pressing Play within a second of the page loading.
    const { at, went } = driven('detail', ITEM)
    const item = await publisher(ITEM, 'Heat')
    await flushPromises()
    expect(screenName.value).toBe('Heat · kahawai')

    at.value = 'player'
    went.value = PLAYING
    await flushPromises()
    // Emptied on arrival, so the words that follow are a change.
    expect(screenName.value).toBe('')
    item.wrapper.unmount()
    await publisher(PLAY, 'Heat')
    await flushPromises()
    expect(screenName.value).toBe('Heat · kahawai')
  })

  test('and the next ITEM is somewhere you went, though the screen is the same', async () => {
    // Pressing an episode on a series page, or a related film: the route name
    // does not change, and armed on that alone the whole navigation was
    // silent. Driven against the running hub, that is most of what a viewer
    // does.
    const { went } = driven('detail', ITEM)
    const first = await publisher(ITEM, 'Heat')
    await flushPromises()
    expect(screenName.value).toBe('Heat · kahawai')
    vi.advanceTimersByTime(1500)

    went.value = OTHER
    await flushPromises()
    // The name of the item being LEFT is still published here, tagged with the
    // address it belongs to, and cannot be mistaken for this one.
    expect(document.title).toBe('kahawai')
    first.wrapper.unmount()
    await publisher(OTHER, 'Sleepers')
    await flushPromises()
    expect(document.title).toBe('Sleepers · kahawai')
    expect(screenName.value).toBe('Sleepers · kahawai')
  })

  test('but a handover is not somewhere you went', async () => {
    // The player's autoplay handover changes the URL and nothing else — the
    // frame does not remount and the focus does not move, so the boundary's
    // key deliberately holds still across it. The announcement follows suit.
    driven('player', PLAYING)
    const first = await publisher(PLAY, 'Blue Exorcist · Episode 1')
    await flushPromises()
    expect(screenName.value).toBe('Blue Exorcist · Episode 1 · kahawai')
    vi.advanceTimersByTime(1500)

    first.wrapper.unmount()
    await publisher(PLAY, 'Blue Exorcist · Episode 2')
    await flushPromises()
    expect(document.title).toBe('Blue Exorcist · Episode 2 · kahawai')
    expect(screenName.value).toBe('')
  })

  test('and a torn-down screen cannot silence the next one', async () => {
    // The announcement is at module scope, because there is one document title
    // and one region — so a pending clear from a screen that is gone lands on
    // whatever is being announced now. Signing out and back in is the way to
    // reach it.
    const first = driven('libraries')
    first.at.value = 'settings'
    first.went.value = 'settings'
    await flushPromises()
    vi.advanceTimersByTime(900)
    first.wrapper.unmount()

    const second = driven('libraries')
    second.at.value = 'admin'
    second.went.value = 'admin'
    await flushPromises()
    expect(screenName.value).toBe('Admin · kahawai')
    // The first screen's clear would have fired here.
    vi.advanceTimersByTime(200)
    expect(screenName.value).toBe('Admin · kahawai')
  })
})

describe('a view naming its own screen', () => {
  test('publishes what it is showing', async () => {
    const { source } = await publisher(ITEM)
    expect(screenShowing.value).toBe(null)
    source.value = 'Heat'
    await flushPromises()
    expect(screenShowing.value).toEqual({ at: ITEM, name: 'Heat' })
  })

  test('and takes it back when it goes', async () => {
    const { wrapper } = await publisher(ITEM, 'Heat')
    expect(screenShowing.value).toEqual({ at: ITEM, name: 'Heat' })
    wrapper.unmount()
    expect(screenShowing.value).toBe(null)
  })

  test('and takes back only its own address’s', async () => {
    // By ADDRESS, not by value. Two screens in a row showing the same name —
    // an item and the player started from it — is the everyday case, and a
    // teardown that compared the text would take the new screen's name away.
    const outgoing = await publisher(ITEM, 'Heat')
    const incoming = await publisher(PLAY, 'Heat')
    outgoing.wrapper.unmount()
    expect(screenShowing.value).toEqual({ at: PLAYING, name: 'Heat' })
    incoming.wrapper.unmount()
    expect(screenShowing.value).toBe(null)
  })

  test('and a view kept across a change of address republishes under the new one', async () => {
    // These components are REUSED: pressing a related item swaps the id and
    // the same `Detail` answers for both. The address has to be read when the
    // name is published, not when the view was created, or the second item's
    // title is filed under the first one's address and never reaches the tab
    // strip at all.
    const { source, router } = await publisher(ITEM, 'Heat')
    expect(screenShowing.value).toEqual({ at: ITEM, name: 'Heat' })

    await router.push(OTHER)
    source.value = 'Sleepers'
    await flushPromises()
    expect(screenShowing.value).toEqual({ at: OTHER, name: 'Sleepers' })
  })

  test('and a name of nothing but spaces is no name', async () => {
    const { source } = await publisher(ITEM, 'Heat')
    source.value = '   '
    await flushPromises()
    expect(screenShowing.value).toBe(null)
  })
})
