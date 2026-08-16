/// HUB-11: the hub's invalidation hints.
///
/// The poll is the safety net; this is what makes a scan's progress and a
/// satellite asking to be let in arrive when they happen rather than up to
/// fifteen seconds later. Every test here is about NOT asking: hints arrive in
/// bursts, and the first version of this re-read everything on every one.

import { QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { defineComponent, h } from 'vue'

vi.mock('../src/api/generated/kahawai.ts', () => ({ getEventsUrl: () => '/api/v1/events' }))

const { DEBOUNCE_MS, useHints } = await import('../src/composables/hints.ts')

/// A stand-in for the browser's, with a handle on the message pump.
class FakeSource {
  static live: FakeSource[] = []
  onmessage: ((event: { data: string }) => void) | null = null
  closed = false
  constructor(public url: string) {
    FakeSource.live.push(this)
  }
  close() {
    this.closed = true
  }
  /// A closed channel delivers nothing, which is what the real one does and
  /// what the disposer relies on.
  send(kind: string) {
    this.raw(JSON.stringify({ kind }))
  }
  raw(data: string) {
    if (!this.closed) this.onmessage?.({ data })
  }
}

let client: QueryClient
let asked: string[]

/// Mount something that listens, and record what it invalidates.
function listening(sections: { always: string[]; quiet?: string[] }) {
  client = new QueryClient()
  asked = []
  vi.spyOn(client, 'invalidateQueries').mockImplementation(async (filter) => {
    const key = (filter as { queryKey?: string[] } | undefined)?.queryKey ?? []
    asked.push(key.join('/'))
  })
  const wrapper = mount(
    defineComponent({
      setup() {
        useHints(sections)
        return () => h('div')
      },
    }),
    { global: { plugins: [[VueQueryPlugin, { queryClient: client }]] } },
  )
  return { wrapper, source: FakeSource.live.at(-1)! }
}

beforeEach(() => {
  FakeSource.live = []
  vi.stubGlobal('EventSource', FakeSource)
  vi.useFakeTimers()
})
afterEach(() => {
  vi.useRealTimers()
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
})

describe('listening for hints', () => {
  test('re-reads what a hint touches, once the burst has settled', () => {
    const { source } = listening({ always: ['satellites'], quiet: ['users'] })
    source.send('satellite')
    expect(asked).toEqual([])
    vi.advanceTimersByTime(DEBOUNCE_MS)
    expect(asked).toEqual(['admin/satellites', 'admin/users'])
  })

  test('and a burst of them is one re-read, not one each', () => {
    // The hub emits a hint every five hundred files during a scan.
    const { source } = listening({ always: ['collections'] })
    for (let n = 0; n < 20; n++) source.send('scan')
    vi.advanceTimersByTime(DEBOUNCE_MS * 4)
    expect(asked).toEqual(['admin/collections'])
  })

  test('a scan cannot change an account or a credential, so it does not ask', () => {
    // Every scan hint used to re-read the users and the provider credentials as
    // well: eight requests a burst, for the whole of a scan.
    const { source } = listening({
      always: ['satellites', 'libraries', 'collections'],
      quiet: ['users', 'providers'],
    })
    source.send('scan')
    vi.advanceTimersByTime(DEBOUNCE_MS)
    expect(asked).toEqual(['admin/libraries', 'admin/collections'])
  })

  test('but anything else does', () => {
    const { source } = listening({ always: ['satellites'], quiet: ['users'] })
    source.send('enrollment')
    vi.advanceTimersByTime(DEBOUNCE_MS)
    expect(asked).toContain('admin/users')
  })

  test('a malformed hint is ignored rather than thrown', () => {
    const { source } = listening({ always: ['satellites'] })
    expect(() => source.raw('not json')).not.toThrow()
    vi.advanceTimersByTime(DEBOUNCE_MS)
    expect(asked).toEqual([])
  })

  test('and leaving the screen closes the channel', () => {
    const { wrapper, source } = listening({ always: ['satellites'] })
    wrapper.unmount()
    expect(source.closed).toBe(true)
    // ...and a hint already in the debounce does not land afterwards.
    source.send('satellite')
    vi.advanceTimersByTime(DEBOUNCE_MS * 4)
    expect(asked).toEqual([])
  })

  test('and an environment with no EventSource simply polls', () => {
    // This is an optimisation over polling, not the only way anything updates.
    vi.stubGlobal('EventSource', undefined)
    expect(() => listening({ always: ['satellites'] })).not.toThrow()
  })
})
