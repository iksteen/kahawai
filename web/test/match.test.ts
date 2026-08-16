/// HUB-8 hand-matching, from the grid.
///
/// The subject of every test here is the FILE identity: the dialog exists
/// because the displayed title may be wrong, so anything it takes from the
/// display rather than from the file is the bug it was written against.

import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

import type { ItemRowI64 } from '../src/api/generated/model/itemRowI64.ts'
import { ApiError } from '../src/api/errors.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  adminApplyMatch: vi.fn(),
  adminReviewSearch: vi.fn(),
  // The card reaches for artwork, and a partial mock of a generated module is
  // a missing-export error rather than a passthrough.
  getItemArtworkUrl: (id: string) => `/art/${id}`,
}))

const api = await import('../src/api/generated/kahawai.ts')
const MatchDialog = (await import('../src/components/MatchDialog.vue')).default
const Card = (await import('../src/components/Card.vue')).default

const item = (over: Record<string, unknown> = {}) => ({
  id: 'i1',
  kind: 'movie',
  title: 'Heat 2 (fan edit)',
  year: 2022,
  file_title: 'Heat',
  file_year: 1995,
  matched_title: 'Heat 2 (fan edit)',
  match_confidence: 'auto',
  ...over,
})

const candidate = (over: Record<string, unknown> = {}) => ({
  id: 949,
  provider: 'tmdb',
  title: 'Heat',
  release_date: '1995-12-15',
  poster_path: '/x.jpg',
  poster_url: 'https://example.invalid/x.jpg',
  overview: null,
  original_language: 'en',
  original_title: 'Heat',
  vote_average: 8.3,
  format: 'movie',
  ...over,
})

/// An answer somebody else decides when to give.
function held<T>(value: T) {
  let settle!: () => void
  const promise = new Promise<T>((resolve) => {
    settle = () => resolve(value)
  })
  return { promise, settle }
}

const open = async (over: Record<string, unknown> = {}) => {
  const wrapper = mount(MatchDialog, { attachTo: document.body, props: { item: item(over) } })
  await flushPromises()
  return wrapper
}

beforeEach(() => {
  vi.mocked(api.adminReviewSearch).mockResolvedValue({ candidates: [candidate()] } as never)
  vi.mocked(api.adminApplyMatch).mockResolvedValue({ ok: true } as never)
})
afterEach(() => vi.resetAllMocks())

describe('the dialog', () => {
  test('searches the FILE’s title and year, not the match being judged', async () => {
    // The display title is the (possibly wrong) match. Searching for it finds
    // the wrong film again, and confirms it.
    await open()
    expect(api.adminReviewSearch).toHaveBeenCalledWith({
      kind: 'movie',
      query: 'Heat',
      year: 1995,
      item: 'i1',
    })
  })

  test('and says which of the two titles it is anchored on', async () => {
    const wrapper = await open()
    expect(wrapper.text()).toContain('Match “Heat” (1995)')
    expect(wrapper.text()).toContain('anchored on the file identity')
  })

  test('a file with no parsed title falls back to the displayed one', async () => {
    await open({ file_title: null, file_year: null })
    expect(api.adminReviewSearch).toHaveBeenCalledWith({
      kind: 'movie',
      query: 'Heat 2 (fan edit)',
      year: null,
      item: 'i1',
    })
  })

  test('picking a candidate sends its provider alongside it', async () => {
    const wrapper = await open()
    await wrapper.find('ul button').trigger('click')
    await flushPromises()
    expect(api.adminApplyMatch).toHaveBeenCalledWith('i1', {
      action: 'pick',
      provider: 'tmdb',
      candidate: candidate(),
    })
    expect(wrapper.emitted('applied')).toHaveLength(1)
    expect(wrapper.emitted('close')).toHaveLength(1)
  })

  test('a refusal is reported and the dialog stays open', async () => {
    vi.mocked(api.adminApplyMatch).mockRejectedValue(new ApiError(409, 'no such candidate'))
    const wrapper = await open()
    await wrapper.find('ul button').trigger('click')
    await flushPromises()
    expect(wrapper.find('[role="alert"]').text()).toContain('no such candidate')
    expect(wrapper.emitted('close')).toBeUndefined()
  })

  test('a failed search says so instead of showing an empty grid', async () => {
    // "no candidates" and "the provider did not answer" are different, and
    // only one of them is a reason to try a different query.
    vi.mocked(api.adminReviewSearch).mockRejectedValue(new ApiError(503, 'provider is away'))
    const wrapper = await open()
    expect(wrapper.find('[role="alert"]').text()).toContain('provider is away')
    expect(wrapper.text()).not.toContain('no candidates')
  })

  test('and no candidates says that', async () => {
    vi.mocked(api.adminReviewSearch).mockResolvedValue({ candidates: [] } as never)
    expect((await open()).text()).toContain('no candidates')
  })

  test('an older search does not replace a newer one’s candidates', async () => {
    // The grid an operator clicks is the one they are looking at, and clicking
    // it APPLIES a match.
    const first = held<{ candidates: unknown[] }>({ candidates: [candidate({ title: 'OLD' })] })
    const second = held<{ candidates: unknown[] }>({ candidates: [candidate({ title: 'NEW' })] })
    vi.mocked(api.adminReviewSearch)
      .mockReturnValueOnce(first.promise as never)
      .mockReturnValueOnce(second.promise as never)
    const wrapper = mount(MatchDialog, { attachTo: document.body, props: { item: item() } })
    await wrapper.find('#match-query').setValue('newer')
    await wrapper.find('form').trigger('submit')

    second.settle()
    await flushPromises()
    expect(wrapper.text()).toContain('NEW')
    first.settle()
    await flushPromises()
    expect(wrapper.text()).toContain('NEW')
    expect(wrapper.text()).not.toContain('OLD')
  })

  test('and Enter on the same text twice is one request', async () => {
    // `:disabled` on the submit button does not stop Enter in the field, and
    // provider search is rate-limited upstream.
    const slow = held<{ candidates: unknown[] }>({ candidates: [] })
    vi.mocked(api.adminReviewSearch).mockReturnValue(slow.promise as never)
    const wrapper = mount(MatchDialog, { attachTo: document.body, props: { item: item() } })
    await wrapper.find('form').trigger('submit')
    await wrapper.find('form').trigger('submit')
    expect(vi.mocked(api.adminReviewSearch).mock.calls).toHaveLength(1)
    slow.settle()
    await flushPromises()
  })

  test('but a different one supersedes it rather than being swallowed', async () => {
    const slow = held<{ candidates: unknown[] }>({ candidates: [] })
    vi.mocked(api.adminReviewSearch).mockReturnValue(slow.promise as never)
    const wrapper = mount(MatchDialog, { attachTo: document.body, props: { item: item() } })
    await wrapper.find('#match-query').setValue('something else')
    await wrapper.find('form').trigger('submit')
    expect(vi.mocked(api.adminReviewSearch).mock.calls).toHaveLength(2)
    slow.settle()
    await flushPromises()
  })

  test('a poster the browser cannot fetch gets the swell', async () => {
    // Otherwise it is the browser's broken-image glyph, in a grid of posters.
    const wrapper = await open()
    await wrapper.find('img').trigger('error')
    expect(wrapper.find('img').exists()).toBe(false)
    expect(wrapper.find('.ghost-art').exists()).toBe(true)
  })

  test('searching again uses what was typed', async () => {
    const wrapper = await open()
    await wrapper.find('#match-query').setValue('Heat 1995 remaster')
    await wrapper.find('form').trigger('submit')
    await flushPromises()
    expect(vi.mocked(api.adminReviewSearch).mock.calls[1]![0]).toMatchObject({
      query: 'Heat 1995 remaster',
    })
  })
})

describe('an uncertain match', () => {
  test('offers confirm and reject, naming what would be confirmed', async () => {
    const wrapper = await open({ match_confidence: 'weak' })
    expect(wrapper.text()).toContain('Uncertain match')
    expect(wrapper.text()).toContain('Heat 2 (fan edit)')

    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Confirm current')!
      .trigger('click')
    await flushPromises()
    expect(api.adminApplyMatch).toHaveBeenCalledWith('i1', {
      action: 'confirm',
      provider: null,
      candidate: null,
    })
  })

  test('and a certain one does not', async () => {
    expect((await open()).text()).not.toContain('Uncertain match')
  })
})

describe('the dialog’s keyboard', () => {
  test('Escape closes it', async () => {
    const wrapper = await open()
    await wrapper.find('[role="dialog"]').trigger('keydown', { key: 'Escape' })
    expect(wrapper.emitted('close')).toHaveLength(1)
  })

  test('and focus starts in the search box', async () => {
    const wrapper = await open()
    expect(document.activeElement).toBe(wrapper.find('#match-query').element)
  })

  test('Escape works wherever the focus is, including nowhere', async () => {
    // Clicking any prose in the dialog puts the focus on <body>, where a
    // handler bound to the dialog's own subtree never sees the key.
    const wrapper = await open()
    ;(document.activeElement as HTMLElement | null)?.blur()
    document.body.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
    expect(wrapper.emitted('close')).toHaveLength(1)
  })

  test('it says it is a modal', async () => {
    const wrapper = await open()
    expect(wrapper.find('[role="dialog"]').attributes('aria-modal')).toBe('true')
  })

  test('and Tab stays inside it', async () => {
    // A dialog whose focus wanders onto the page behind it is a dialog only
    // for people using a mouse.
    const wrapper = await open()
    const stops = wrapper.findAll('button, input')
    const last = stops.at(-1)!.element as HTMLElement
    last.focus()
    last.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', bubbles: true }))
    expect(document.activeElement).toBe(stops[0]!.element)

    const first = stops[0]!.element as HTMLElement
    first.focus()
    first.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, shiftKey: true }))
    expect(document.activeElement).toBe(last)
  })

  test('and is given back to whatever opened it', async () => {
    const opener = document.createElement('button')
    document.body.append(opener)
    opener.focus()
    const wrapper = await open()
    wrapper.unmount()
    expect(document.activeElement).toBe(opener)
    opener.remove()
  })
})

describe('the card’s match affordance', () => {
  const row = (over: Record<string, unknown> = {}) =>
    ({
      id: 'i1',
      kind: 'movie',
      title: 'Heat',
      played: false,
      art_version: 1,
      ...over,
    }) as unknown as ItemRowI64 & { played: boolean }

  test('is not offered unless the caller says so', async () => {
    // Only an admin has the endpoint, and only a work has an identity of its
    // own to match — an episode inherits its show's.
    const wrapper = mount(Card, { props: { item: row() } })
    expect(wrapper.findAll('button')).toHaveLength(1)
  })

  const mark = (confidence: string | null) =>
    mount(Card, {
      props: { item: row({ match_confidence: confidence }), matchable: true },
    }).findAll('button')[0]!

  test('says which of the three jobs it is', () => {
    expect(mark('weak').attributes('title')).toContain('Uncertain')
    expect(mark('auto').attributes('title')).toContain('Re-match')
    expect(mark('manual').attributes('title')).toContain('Re-match')
    expect(mark(null).attributes('title')).toContain('No metadata match')
    expect(mark('rejected').attributes('title')).toContain('No metadata match')
  })

  test('and colours them apart, because that is what a grid is scanned for', () => {
    // Three jobs, three readings: nothing matched (fix it), matched but
    // uncertain (review it), matched (re-match if you disagree).
    expect(mark(null).classes()).toContain('text-warn')
    expect(mark('weak').classes()).toContain('text-sand')
    expect(mark('auto').classes()).toContain('text-dim')
  })

  test('and only the two that need attention are always visible', () => {
    // A magnifier on every one of two thousand matched cards is noise; on
    // hover and on keyboard focus is not.
    expect(mark('auto').classes()).toContain('opacity-0')
    expect(mark('auto').classes()).toContain('focus-visible:opacity-100')
    expect(mark('weak').classes()).not.toContain('opacity-0')
    expect(mark(null).classes()).not.toContain('opacity-0')
  })

  test('and names the item it is about, for whoever cannot see the grid', () => {
    const wrapper = mount(Card, { props: { item: row(), matchable: true } })
    expect(wrapper.findAll('button')[0]!.attributes('aria-label')).toContain('Heat')
  })

  test('asking to match does not open the item', async () => {
    const wrapper = mount(Card, { props: { item: row(), matchable: true } })
    await wrapper.findAll('button')[0]!.trigger('click')
    expect(wrapper.emitted('match')).toHaveLength(1)
    expect(wrapper.emitted('open')).toBeUndefined()
  })
})
