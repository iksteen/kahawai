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
function driven(route: Screen) {
  const at = ref(route)
  const wrapper = mount(
    defineComponent({
      setup() {
        useDocumentTitle(at)
        return () => h('div')
      },
    }),
  )
  mounted.push(wrapper)
  return { at, wrapper }
}

/// A screen publishing its own name, the way a view does. Separate from
/// `driven` so a test can land the name late, or take the screen away, or —
/// the case that matters — leave the old one standing across a route change.
function publisher(screen: Screen, name: string | null = null) {
  const source = ref<string | null>(name)
  const wrapper = mount(
    defineComponent({
      setup() {
        useScreenName(screen, source)
        return () => h('div')
      },
    }),
  )
  mounted.push(wrapper)
  return { source, wrapper }
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

describe('moving between screens', () => {
  test('sets the document title at once', async () => {
    const { at } = driven('libraries')
    expect(document.title).toBe('Home · kahawai')
    at.value = 'settings'
    await flushPromises()
    expect(document.title).toBe('Settings · kahawai')
  })

  test('and says the same words out loud', async () => {
    // A title change alone is announced by some screen readers and not others.
    const { at } = driven('libraries')
    at.value = 'settings'
    await flushPromises()
    expect(screenName.value).toBe('Settings · kahawai')
  })

  test('and stops saying them, so the next visit is announced too', async () => {
    // A live region only speaks when its content CHANGES: left standing, going
    // back to a screen you have already been on says nothing at all.
    const { at } = driven('libraries')
    at.value = 'settings'
    await flushPromises()
    vi.advanceTimersByTime(1500)
    expect(screenName.value).toBe('')

    at.value = 'libraries'
    await flushPromises()
    at.value = 'settings'
    await flushPromises()
    expect(screenName.value).toBe('Settings · kahawai')
  })

  test('and the title follows the name arriving late', async () => {
    // The name is a round trip behind the route. A screen with a placeholder
    // shows it meanwhile — a tab strip cannot wait — but says nothing:
    // "Library" is the word the announcement exists to replace, and a screen
    // is only announced once, so saying it spends the answer to "where am I"
    // on the question.
    const { at } = driven('libraries')
    vi.advanceTimersByTime(1500)
    at.value = 'library'
    await flushPromises()
    expect(document.title).toBe('Library · kahawai')
    expect(screenName.value).toBe('')

    publisher('library', 'Films')
    await flushPromises()
    expect(document.title).toBe('Films · kahawai')
    expect(screenName.value).toBe('Films · kahawai')
  })

  test('and a screen titled by its contents waits for them', async () => {
    // The item's title is a round trip behind the route, and this screen has no
    // word of its own to fall back on. Announcing on arrival announced the
    // literal word "kahawai" — and then the real title landed under the
    // once-per-screen guard and was never said at all.
    const { at } = driven('libraries')
    vi.advanceTimersByTime(1500)
    at.value = 'detail'
    await flushPromises()
    expect(document.title).toBe('kahawai')
    expect(screenName.value).toBe('')

    publisher('detail', 'Heat')
    await flushPromises()
    expect(document.title).toBe('Heat · kahawai')
    expect(screenName.value).toBe('Heat · kahawai')
  })

  test('and still says it only once', async () => {
    const { at } = driven('libraries')
    at.value = 'detail'
    await flushPromises()
    const item = publisher('detail', 'Heat')
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
    const { at } = driven('library')
    const library = publisher('library', 'Films')
    await flushPromises()
    vi.advanceTimersByTime(1500)

    at.value = 'detail'
    await flushPromises()
    expect(document.title).toBe('kahawai')
    expect(screenName.value).toBe('')

    // Only now does the outgoing screen go, which is the order Vue uses.
    library.wrapper.unmount()
    publisher('detail', 'Heat')
    await flushPromises()
    expect(document.title).toBe('Heat · kahawai')
    expect(screenName.value).toBe('Heat · kahawai')
  })

  test('and the next screen is announced even with the SAME words', async () => {
    // Pressing Play on an item page: two screens, one name. A live region only
    // speaks when its content changes, so with the item page's sentence still
    // standing, setting the identical string said nothing — and the everyday
    // path is pressing Play within a second of the page loading.
    const { at } = driven('detail')
    const item = publisher('detail', 'Heat')
    await flushPromises()
    expect(screenName.value).toBe('Heat · kahawai')

    at.value = 'player'
    await flushPromises()
    // Emptied on arrival, so the words that follow are a change.
    expect(screenName.value).toBe('')
    item.wrapper.unmount()
    publisher('player', 'Heat')
    await flushPromises()
    expect(screenName.value).toBe('Heat · kahawai')
  })

  test('and a torn-down screen cannot silence the next one', async () => {
    // The announcement is at module scope, because there is one document title
    // and one region — so a pending clear from a screen that is gone lands on
    // whatever is being announced now. Signing out and back in is the way to
    // reach it.
    const first = driven('libraries')
    first.at.value = 'settings'
    await flushPromises()
    vi.advanceTimersByTime(900)
    first.wrapper.unmount()

    const second = driven('libraries')
    second.at.value = 'admin'
    await flushPromises()
    expect(screenName.value).toBe('Admin · kahawai')
    // The first screen's clear would have fired here.
    vi.advanceTimersByTime(200)
    expect(screenName.value).toBe('Admin · kahawai')
  })
})

describe('a view naming its own screen', () => {
  test('publishes what it is showing', async () => {
    const { source } = publisher('detail')
    expect(screenShowing.value).toBe(null)
    source.value = 'Heat'
    await flushPromises()
    expect(screenShowing.value).toEqual({ screen: 'detail', name: 'Heat' })
  })

  test('and takes it back when it goes', async () => {
    const { wrapper } = publisher('detail', 'Heat')
    expect(screenShowing.value).toEqual({ screen: 'detail', name: 'Heat' })
    wrapper.unmount()
    expect(screenShowing.value).toBe(null)
  })

  test('and takes back only its own screen’s', async () => {
    // By SCREEN, not by value. Two screens in a row showing the same name —
    // an item and the player started from it — is the everyday case, and a
    // teardown that compared the text would take the new screen's name away.
    const outgoing = publisher('detail', 'Heat')
    const incoming = publisher('player', 'Heat')
    outgoing.wrapper.unmount()
    expect(screenShowing.value).toEqual({ screen: 'player', name: 'Heat' })
    incoming.wrapper.unmount()
    expect(screenShowing.value).toBe(null)
  })

  test('and a name of nothing but spaces is no name', async () => {
    const { source } = publisher('detail', 'Heat')
    source.value = '   '
    await flushPromises()
    expect(screenShowing.value).toBe(null)
  })
})
