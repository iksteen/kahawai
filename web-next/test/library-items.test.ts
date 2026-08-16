/// A library's items, a chunk at a time. Most of these are about a chunk that
/// failed beside chunks that did not: the grid is a reserved height full of
/// placeholders, so a hole in it looks exactly like something still loading.

import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { defineComponent, h, ref } from 'vue'

import type { ItemRowI64 } from '../src/api/generated/model/itemRowI64.ts'
import { ApiError } from '../src/api/errors.ts'
import { CHUNK } from '../src/domain/virtual.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({ listItems: vi.fn() }))

const { listItems } = await import('../src/api/generated/kahawai.ts')
const { useLibraryItems } = await import('../src/composables/library.ts')

const item = (id: string) => ({ id, title: id }) as ItemRowI64

/// A library of `total` items, answering every chunk — except the ones named.
function hub(total: number, { failing = [] as number[] } = {}) {
  vi.mocked(listItems).mockImplementation(async (params) => {
    const offset = params?.offset ?? 0
    if (failing.includes(offset / CHUNK)) throw new ApiError(503, 'the hub is restarting')
    const items = Array.from({ length: Math.min(CHUNK, total - offset) }, (_, n) =>
      item(`i${offset + n}`),
    )
    return { items, total, limit: CHUNK, offset }
  })
}

function driven(library = 'films', query = '', sort = 'title') {
  const refs = { library: ref(library), query: ref(query), sort: ref(sort) }
  let api!: ReturnType<typeof useLibraryItems>
  const wrapper = mount(
    defineComponent({
      setup() {
        api = useLibraryItems(refs.library, refs.query, refs.sort)
        return () => h('div')
      },
    }),
  )
  return { ...refs, api: () => api, wrapper }
}

beforeEach(() => hub(1000))
afterEach(() => vi.resetAllMocks())

describe('opening a library', () => {
  test('asks for the first chunk and nothing else', async () => {
    const { api } = driven()
    await flushPromises()
    expect(listItems).toHaveBeenCalledTimes(1)
    expect(api().total.value).toBe(1000)
    expect(api().loaded.value.size).toBe(CHUNK)
  })

  test('and the rest arrive keyed by their place in the library', async () => {
    // Sparse and keyed by index: the grid reserves the whole height from the
    // first answer, and most of it has never been fetched.
    const { api } = driven()
    await flushPromises()
    api().need([4])
    await flushPromises()
    expect(api().loaded.value.get(400)?.id).toBe('i400')
    expect(api().loaded.value.has(300)).toBe(false)
  })

  test('a chunk already held is not asked for twice', async () => {
    const { api } = driven()
    await flushPromises()
    api().need([0])
    api().need([0])
    await flushPromises()
    expect(listItems).toHaveBeenCalledTimes(1)
  })
})

describe('a chunk that failed', () => {
  test('says so, and can be asked again', async () => {
    hub(1000, { failing: [4] })
    const { api } = driven()
    await flushPromises()
    api().need([4])
    await flushPromises()
    expect(api().failure.value).toContain('restarting')

    hub(1000)
    api().retry()
    await flushPromises()
    expect(api().failure.value).toBe('')
    expect(api().loaded.value.get(400)?.id).toBe('i400')
  })

  test('and scrolling back to it asks again, without pressing anything', async () => {
    // A chunk that failed has to leave the asked set, or the only way to fill
    // that hole for the rest of the session is the button.
    hub(1000, { failing: [4] })
    const { api } = driven()
    await flushPromises()
    api().need([4])
    await flushPromises()
    expect(api().loaded.value.has(400)).toBe(false)

    hub(1000)
    api().need([4])
    await flushPromises()
    expect(api().loaded.value.get(400)?.id).toBe('i400')
  })

  test('and the line stays up while another chunk is still missing', async () => {
    // Clearing on ANY arrival hides a real hole: one chunk failing beside one
    // succeeding leaves a hundred placeholder cards and silence.
    hub(1000, { failing: [4, 5] })
    const { api } = driven()
    await flushPromises()
    api().need([4, 5])
    await flushPromises()
    expect(api().failure.value).not.toBe('')

    // Only chunk 4 recovers.
    hub(1000, { failing: [5] })
    api().retry()
    await flushPromises()
    expect(api().loaded.value.get(400)?.id).toBe('i400')
    expect(api().failure.value).not.toBe('')
  })

  test('and a different chunk arriving does not clear it', async () => {
    // The line is about the SET of failures being non-empty, not about the
    // last thing that happened: one chunk failing beside one succeeding is a
    // hundred placeholder cards and silence.
    hub(1000, { failing: [4] })
    const { api } = driven()
    await flushPromises()
    api().need([4])
    await flushPromises()
    expect(api().failure.value).not.toBe('')

    // Chunk 6 loads perfectly while 4 is still missing.
    api().need([6])
    await flushPromises()
    expect(api().loaded.value.has(600)).toBe(true)
    expect(api().failure.value).not.toBe('')
  })

  test('the first chunk failing is still retryable, though nothing asks for it', async () => {
    // There is no `total` yet, so the grid reserves nothing and no visible row
    // will ask — the retry has to ask for chunk 0 by hand.
    hub(1000, { failing: [0] })
    const { api } = driven()
    await flushPromises()
    expect(api().total.value).toBeNull()
    expect(api().failure.value).not.toBe('')

    hub(1000)
    api().retry()
    // Cleared straight away, not when the answer lands: the line stays on
    // screen for a whole round trip otherwise, over a request that is out.
    expect(api().failure.value).toBe('')
    await flushPromises()
    expect(api().total.value).toBe(1000)
    expect(api().failure.value).toBe('')
  })
})

describe('changing what is being looked at', () => {
  test('a filter replaces the items rather than merging into them', async () => {
    const { api, query } = driven()
    await flushPromises()
    api().need([4])
    await flushPromises()
    expect(api().loaded.value.size).toBe(2 * CHUNK)

    hub(3)
    query.value = 'heat'
    await flushPromises()
    // Everything the old result set held is gone: index 400 belongs to a list
    // nobody is looking at any more.
    expect(api().loaded.value.size).toBe(3)
    expect(api().loaded.value.has(400)).toBe(false)
  })

  test('and what was on screen stays there until the new answer lands', async () => {
    // Blanking on the keystroke empties the page for the length of a round
    // trip, and an empty page is a different tree.
    const { api, query } = driven()
    await flushPromises()

    let answer = () => {}
    vi.mocked(listItems).mockReturnValue(
      new Promise(
        (resolve) => (answer = () => resolve({ items: [], total: 0, limit: CHUNK, offset: 0 })),
      ),
    )
    query.value = 'heat'
    await flushPromises()
    expect(api().loaded.value.size).toBe(CHUNK)

    answer()
    await flushPromises()
    expect(api().loaded.value.size).toBe(0)
  })

  test('an answer to the question you have left never paints', async () => {
    // A reply carrying an older generation describes a library or a search we
    // are no longer on.
    const { api, query } = driven()
    await flushPromises()

    let answerOld = () => {}
    vi.mocked(listItems).mockReturnValueOnce(
      new Promise((resolve) => {
        answerOld = () => resolve({ items: [item('stale')], total: 1, limit: CHUNK, offset: 0 })
      }),
    )
    query.value = 'first'
    await flushPromises()

    hub(2)
    query.value = 'second'
    await flushPromises()
    expect(api().total.value).toBe(2)

    answerOld()
    await flushPromises()
    expect(api().total.value).toBe(2)
    expect(api().loaded.value.get(0)?.id).not.toBe('stale')
  })

  test('and a failure from it does not either', async () => {
    // The failures belonged to the result set being replaced. Left standing,
    // the line sits over results that loaded perfectly.
    const { api, query } = driven()
    await flushPromises()

    let refuseOld = () => {}
    vi.mocked(listItems).mockReturnValueOnce(
      new Promise((_resolve, reject) => (refuseOld = () => reject(new ApiError(500, 'gone')))),
    )
    query.value = 'first'
    await flushPromises()

    hub(2)
    query.value = 'second'
    await flushPromises()

    refuseOld()
    await flushPromises()
    expect(api().failure.value).toBe('')
  })
})

describe('the library’s own size', () => {
  test('is remembered, so a filtered count can say what it filtered from', async () => {
    const { api, query } = driven()
    await flushPromises()
    expect(api().libraryTotal.value).toBe(1000)

    hub(12)
    query.value = 'heat'
    await flushPromises()
    expect(api().total.value).toBe(12)
    // A filtered answer does not overwrite it: 12 of 1000 is the whole point.
    expect(api().libraryTotal.value).toBe(1000)
  })

  test('and is forgotten when the library changes', async () => {
    // With a filter standing across the switch, which is the only case the
    // line matters in: an unfiltered answer re-sets it anyway.
    const { api, library, query } = driven()
    await flushPromises()
    hub(12)
    query.value = 'heat'
    await flushPromises()
    expect(api().libraryTotal.value).toBe(1000)

    hub(3)
    library.value = 'music'
    await flushPromises()
    // The new library has not said how big it is — the answer was filtered.
    expect(api().libraryTotal.value).toBeNull()
  })

  test('a chunk that lands after the page has gone paints nothing', async () => {
    // Whatever is in flight when the view goes away must not write to refs
    // nothing is rendering.
    let answer = () => {}
    vi.mocked(listItems).mockReturnValue(
      new Promise((resolve) => {
        answer = () => resolve({ items: [item('late')], total: 1, limit: CHUNK, offset: 0 })
      }),
    )
    const { api, wrapper } = driven()
    await flushPromises()
    wrapper.unmount()

    answer()
    await flushPromises()
    expect(api().loaded.value.size).toBe(0)
    expect(api().total.value).toBeNull()
  })
})
