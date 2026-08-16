/// The search panel as the keyboard reaches it: through the box it belongs
/// to, and nowhere else. Scoping these to the search area rather than the
/// window is what keeps them from arguing with the menus, the dialogs and the
/// player's own Escape.

import { QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { createMemoryHistory, createRouter } from 'vue-router'

import type { ItemRowI64 } from '../src/api/generated/model/itemRowI64.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  listItems: vi.fn(),
  getItemArtworkUrl: (id: string) => `/api/v1/items/${id}/artwork`,
}))

const { listItems } = await import('../src/api/generated/kahawai.ts')
const AppShell = (await import('../src/components/AppShell.vue')).default
const { DEBOUNCE_MS } = await import('../src/composables/search.ts')

const LIBS = [
  { id: 'films', name: 'Films', media_type: 'movies' },
  { id: 'music', name: 'Music', media_type: 'music' },
]
const item = (id: string) => ({ id, title: id, kind: 'movie' }) as ItemRowI64

function answers(by: Record<string, ItemRowI64[]>) {
  vi.mocked(listItems).mockImplementation(async (params) => ({
    items: by[params?.library ?? ''] ?? [],
    total: (by[params?.library ?? ''] ?? []).length,
    limit: 5,
    offset: 0,
  }))
}

const Blank = { template: '<div />' }

async function shell() {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/', name: 'libraries', component: Blank },
      { path: '/library/:library', name: 'library', component: Blank },
      { path: '/library/:library/item/:id', name: 'detail', component: Blank },
    ],
  })
  await router.push('/')
  await router.isReady()
  const wrapper = mount(AppShell, {
    props: { libraries: LIBS, username: 'claude', admin: false },
    global: {
      plugins: [
        router,
        [
          VueQueryPlugin,
          { queryClient: new QueryClient({ defaultOptions: { queries: { retry: false } } }) },
        ] as [typeof VueQueryPlugin, { queryClient: QueryClient }],
      ],
    },
    attachTo: document.body,
  })
  return { router, wrapper }
}

/// Type into the box and let the debounce and the search settle.
async function search(wrapper: Awaited<ReturnType<typeof shell>>['wrapper'], text: string) {
  await wrapper.find('input').setValue(text)
  vi.advanceTimersByTime(DEBOUNCE_MS)
  await flushPromises()
}

const key = (wrapper: Awaited<ReturnType<typeof shell>>['wrapper'], name: string) =>
  wrapper.find('input').trigger('keydown', { key: name })

beforeEach(() => {
  vi.useFakeTimers()
  answers({ films: [item('heat'), item('the insider')], music: [item('heat wave')] })
})
afterEach(() => {
  vi.useRealTimers()
  vi.resetAllMocks()
})

describe('the panel appears when there is something to show', () => {
  test('and the box says so, in the way a combobox has to', async () => {
    const { wrapper } = await shell()
    expect(wrapper.find('input').attributes('aria-expanded')).toBe('false')

    await search(wrapper, 'heat')
    expect(wrapper.find('[role="listbox"]').exists()).toBe(true)
    expect(wrapper.find('input').attributes('aria-expanded')).toBe('true')
    expect(wrapper.find('input').attributes('aria-controls')).toBe(
      wrapper.find('[role="listbox"]').attributes('id'),
    )
  })

  test('a heading per library, then its hits', async () => {
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    const rows = wrapper.findAll('[role="option"]')
    expect(rows.map((r) => r.text().replace(/\s+/g, ' ').trim())).toEqual([
      'Films2',
      'heat',
      'the insider',
      'Music1',
      'heat wave',
    ])
  })
})

describe('walking it', () => {
  test('the arrows light rows without taking the caret out of the box', async () => {
    // That is what `aria-activedescendant` is for: focus stays where somebody
    // is still typing.
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    const input = wrapper.find('input').element
    input.focus()

    await key(wrapper, 'ArrowDown')
    expect(document.activeElement).toBe(input)
    expect(input.getAttribute('aria-activedescendant')).toBe(
      wrapper.findAll('[role="option"]')[0]!.attributes('id'),
    )

    await key(wrapper, 'ArrowDown')
    expect(input.getAttribute('aria-activedescendant')).toBe(
      wrapper.findAll('[role="option"]')[1]!.attributes('id'),
    )
  })

  test('and the lit row is the one marked selected', async () => {
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    await key(wrapper, 'ArrowDown')
    const selected = wrapper
      .findAll('[role="option"]')
      .filter((r) => r.attributes('aria-selected') === 'true')
    expect(selected).toHaveLength(1)
    expect(selected[0]!.text()).toContain('Films')
  })

  test('the rows are not tab stops', async () => {
    // The arrows are how this list is walked and Tab is how you leave it.
    // Nine rows of tab stops between the search box and the rest of the header
    // is not navigation — and focus never leaves the field, which is the whole
    // reason the rows carry ids.
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    for (const row of wrapper.findAll('[role="option"]')) {
      expect(row.attributes('tabindex')).toBe('-1')
    }
  })

  test('up from nothing stays out of the list', async () => {
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    await key(wrapper, 'ArrowUp')
    expect(wrapper.find('input').attributes('aria-activedescendant')).toBeUndefined()
  })

  test('the arrows do not also move the caret', async () => {
    // Without `preventDefault` the caret jumps to one end of the query while
    // the highlight moves, so the next letter typed lands in the wrong place.
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    const event = new KeyboardEvent('keydown', {
      key: 'ArrowDown',
      cancelable: true,
      bubbles: true,
    })
    wrapper.find('input').element.dispatchEvent(event)
    expect(event.defaultPrevented).toBe(true)
  })

  test('and they belong to the field, not to everything in the search area', async () => {
    // Focus can legitimately be on the ✕ or on Try again with the panel up,
    // and there Enter must press that button rather than open a library.
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    await key(wrapper, 'ArrowDown')
    const lit = wrapper.find('input').attributes('aria-activedescendant')

    await wrapper.find('button[title="Clear"]').trigger('keydown', { key: 'ArrowDown' })
    expect(wrapper.find('input').attributes('aria-activedescendant')).toBe(lit)
  })

  test('and a composition keeps its own arrows', async () => {
    // Typing Japanese, the arrows walk the IME's candidate list and Enter
    // commits the word. Take them and choosing a character navigates into a
    // library instead.
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    await wrapper.find('input').trigger('keydown', { key: 'ArrowDown', isComposing: true })
    expect(wrapper.find('input').attributes('aria-activedescendant')).toBeUndefined()
  })
})

describe('pressing one', () => {
  test('Enter with nothing lit opens the first library, not a guess at a film', async () => {
    const { router, wrapper } = await shell()
    await search(wrapper, 'heat')
    await key(wrapper, 'Enter')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/library/films')
  })

  test('a library keeps the text, where it becomes that library’s filter', async () => {
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    await key(wrapper, 'Enter')
    await flushPromises()
    expect((wrapper.find('input').element as HTMLInputElement).value).toBe('heat')
  })

  test('an item takes the text with it, because you asked for that one thing', async () => {
    const { router, wrapper } = await shell()
    await search(wrapper, 'heat')
    await key(wrapper, 'ArrowDown')
    await key(wrapper, 'ArrowDown')
    await key(wrapper, 'Enter')
    await flushPromises()
    expect(router.currentRoute.value.path).toBe('/library/films/item/heat')
    // The item page has no box at all, so the text is checked where it would
    // otherwise be standing: back on the screen that has one.
    await router.push('/')
    await flushPromises()
    expect((wrapper.find('input').element as HTMLInputElement).value).toBe('')
  })
})

describe('a query nobody is searching for any more', () => {
  test('an emptied box does not remember its hits', async () => {
    // `keepPreviousData` hands the last result set to the NEXT key, which is
    // what keeps rows actionable between two keystrokes. Across an emptied
    // box it labelled the panel "Results for zzz" over the hits for "heat" —
    // and two arrow presses and Enter opened a film out of them.
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    expect(wrapper.find('[role="listbox"]').exists()).toBe(true)

    await search(wrapper, '')
    let answer = () => {}
    vi.mocked(listItems).mockReturnValue(
      new Promise(
        (resolve) => (answer = () => resolve({ items: [], total: 0, limit: 5, offset: 0 })),
      ),
    )
    await search(wrapper, 'zzz')
    expect(wrapper.find('[role="listbox"]').exists()).toBe(false)
    expect(wrapper.text()).not.toContain('heat')

    answer()
    await flushPromises()
  })

  test('and the label never names one query over another’s rows', async () => {
    const { wrapper } = await shell()
    await search(wrapper, 'heat')

    let answer = () => {}
    vi.mocked(listItems).mockReturnValue(
      new Promise(
        (resolve) => (answer = () => resolve({ items: [], total: 0, limit: 5, offset: 0 })),
      ),
    )
    await wrapper.find('input').setValue('heat 2')
    vi.advanceTimersByTime(DEBOUNCE_MS)
    await flushPromises()

    // The old rows are still there, deliberately — so the label still names
    // the query they belong to.
    expect(wrapper.find('[role="listbox"]').attributes('aria-label')).toBe('Results for heat')
    answer()
    await flushPromises()
  })
})

describe('the highlight', () => {
  test('always points at a row that is in the document', async () => {
    // `aria-activedescendant` naming an id that is not there announces
    // nothing, and there is no way for a screen reader user to tell.
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    for (let press = 0; press < 8; press++) {
      await key(wrapper, 'ArrowDown')
      const at = wrapper.find('input').attributes('aria-activedescendant')
      if (at === undefined) continue
      expect(document.getElementById(at), `after ${press + 1} presses`).not.toBeNull()
    }
  })

  test('and there is nothing to point at when the panel has no rows', async () => {
    // A panel showing "No matches" is on screen with nothing to walk, and
    // taking the arrows there kills caret movement in the box.
    answers({ films: [], music: [] })
    const { wrapper } = await shell()
    await search(wrapper, 'zzz')
    expect(wrapper.find('[role="listbox"]').exists()).toBe(true)

    const event = new KeyboardEvent('keydown', {
      key: 'ArrowDown',
      cancelable: true,
      bubbles: true,
    })
    wrapper.find('input').element.dispatchEvent(event)
    expect(event.defaultPrevented).toBe(false)
    expect(wrapper.find('input').attributes('aria-activedescendant')).toBeUndefined()
  })
})

describe('leaving it', () => {
  test('Escape puts it away and lets go of the field', async () => {
    // A dropdown dismissed while the caret is still blinking in the box that
    // opened it reads as a box that stopped working.
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    wrapper.find('input').element.focus()

    await key(wrapper, 'Escape')
    expect(wrapper.find('[role="listbox"]').exists()).toBe(false)
    expect(document.activeElement).not.toBe(wrapper.find('input').element)
  })

  test('and Escape does not also reach the field', async () => {
    // Escape in a search field reverts its value in some browsers, and losing
    // the query was not what was asked for.
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    const event = new KeyboardEvent('keydown', { key: 'Escape', cancelable: true, bubbles: true })
    wrapper.find('input').element.dispatchEvent(event)
    expect(event.defaultPrevented).toBe(true)
  })

  test('walking away puts it away too', async () => {
    // Four ways out, and a route change is one of them: the panel is over a
    // screen that is no longer there.
    const { router, wrapper } = await shell()
    await search(wrapper, 'heat')
    expect(wrapper.find('[role="listbox"]').exists()).toBe(true)

    await router.push('/library/films')
    await flushPromises()
    expect(wrapper.find('[role="listbox"]').exists()).toBe(false)
  })

  test('and the walk is abandoned with it', async () => {
    // Dismissed at the eighth hit and refocused, Enter opened that hit instead
    // of the first library — which is what "nothing highlighted" means.
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    await key(wrapper, 'ArrowDown')
    await key(wrapper, 'ArrowDown')
    await key(wrapper, 'Escape')

    await wrapper.find('input').trigger('focus')
    expect(wrapper.find('input').attributes('aria-activedescendant')).toBeUndefined()
  })

  test('focusing the box again brings back the results it already had', async () => {
    // Unmounting threw them away, so focusing re-ran every library's search
    // and showed nothing for a round trip.
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    await key(wrapper, 'Escape')
    vi.mocked(listItems).mockClear()

    await wrapper.find('input').trigger('focus')
    await flushPromises()
    expect(wrapper.find('[role="listbox"]').exists()).toBe(true)
    expect(listItems).not.toHaveBeenCalled()
  })

  test('Escape belongs to the panel, not to every search box', async () => {
    // On a library page the same box filters in place with no panel, and
    // Escape there is the browser's — taking it dropped the caret out of the
    // field for nothing.
    const { wrapper, router } = await shell()
    await router.push('/library/films')
    await flushPromises()
    wrapper.find('input').element.focus()

    const event = new KeyboardEvent('keydown', { key: 'Escape', cancelable: true, bubbles: true })
    wrapper.find('input').element.dispatchEvent(event)
    expect(event.defaultPrevented).toBe(false)
    expect(document.activeElement).toBe(wrapper.find('input').element)
  })

  test('and clearing the box puts it away', async () => {
    const { wrapper } = await shell()
    await search(wrapper, 'heat')
    await search(wrapper, '')
    expect(wrapper.find('[role="listbox"]').exists()).toBe(false)
  })
})
