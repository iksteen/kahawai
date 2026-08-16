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

import type { RouteName } from '../src/domain/routes.ts'
import { documentTitle } from '../src/domain/titles.ts'
import { screenName, useDocumentTitle } from '../src/composables/title.ts'

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
})

/// A component that drives the composable, so the watcher runs in a scope.
function driven(route: RouteName, named: string | null) {
  const at = ref(route)
  const thing = ref<string | null>(named)
  const wrapper = mount(
    defineComponent({
      setup() {
        useDocumentTitle(at, thing)
        return () => h('div')
      },
    }),
  )
  return { at, thing, wrapper }
}

beforeEach(() => {
  vi.useFakeTimers()
  document.title = 'kahawai'
})
afterEach(() => vi.useRealTimers())

describe('moving between screens', () => {
  test('sets the document title at once', async () => {
    const { at } = driven('libraries', null)
    expect(document.title).toBe('Home · kahawai')
    at.value = 'settings'
    await flushPromises()
    expect(document.title).toBe('Settings · kahawai')
  })

  test('and says the same words out loud', async () => {
    // A title change alone is announced by some screen readers and not others.
    const { at } = driven('libraries', null)
    at.value = 'settings'
    await flushPromises()
    expect(screenName.value).toBe('Settings · kahawai')
  })

  test('and stops saying them, so the next visit is announced too', async () => {
    // A live region only speaks when its content CHANGES: left standing, going
    // back to a screen you have already been on says nothing at all.
    const { at } = driven('libraries', null)
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

  test('and the name arriving late retitles without announcing again', async () => {
    // The name is a round trip behind the route, so this fires twice per
    // navigation — and announcing the second is announcing the same screen a
    // beat later, over whatever the reader had moved on to.
    const { at, thing } = driven('libraries', null)
    at.value = 'library'
    await flushPromises()
    expect(screenName.value).toBe('Library · kahawai')
    vi.advanceTimersByTime(1500)

    thing.value = 'Films'
    await flushPromises()
    expect(document.title).toBe('Films · kahawai')
    expect(screenName.value).toBe('')
  })

  test('and a torn-down screen cannot silence the next one', async () => {
    // The announcement is at module scope, because there is one document title
    // and one region — so a pending clear from a screen that is gone lands on
    // whatever is being announced now. Signing out and back in is the way to
    // reach it.
    const first = driven('libraries', null)
    first.at.value = 'settings'
    await flushPromises()
    vi.advanceTimersByTime(900)
    first.wrapper.unmount()

    const second = driven('libraries', null)
    second.at.value = 'admin'
    await flushPromises()
    expect(screenName.value).toBe('Admin · kahawai')
    // The first screen's clear would have fired here.
    vi.advanceTimersByTime(200)
    expect(screenName.value).toBe('Admin · kahawai')
  })
})
