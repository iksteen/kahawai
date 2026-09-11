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
  itemDetail: vi.fn(),
  listItems: vi.fn(),
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
  const source = item(over)
  vi.mocked(api.itemDetail).mockResolvedValue({
    copies: [
      {
        id: 'i1',
        title: source.file_title ?? source.title,
        year: source.file_year,
        match_confidence: source.match_confidence,
        matched_title: source.matched_title,
        matched_year: source.year,
        paths: [],
        assignment: { revision: 4, library_item_ids: ['i1'] },
      },
    ],
  } as never)
  const wrapper = mount(MatchDialog, { attachTo: document.body, props: { item: item(over) } })
  await flushPromises()
  return wrapper
}

beforeEach(() => {
  vi.mocked(api.listItems).mockResolvedValue({ items: [] } as never)
  vi.mocked(api.itemDetail).mockResolvedValue({
    copies: [
      {
        id: 'i1',
        title: 'Heat',
        year: 1995,
        paths: [],
        assignment: { revision: 4, library_item_ids: ['i1'] },
      },
    ],
  } as never)
  vi.mocked(api.adminReviewSearch).mockResolvedValue({ candidates: [candidate()] } as never)
  vi.mocked(api.adminApplyMatch).mockResolvedValue({
    library_item_ids: ['i1'],
    revision: 5,
  } as never)
})
afterEach(() => vi.resetAllMocks())

describe('the dialog', () => {
  const selectYearlessCopy = async (initialCopy?: string) => {
    vi.mocked(api.itemDetail).mockResolvedValue({
      copies: [
        {
          id: 'dated',
          title: 'Heat',
          year: 1995,
          paths: ['Heat (1995).mkv'],
          assignment: { revision: 4, library_item_ids: ['i1'] },
        },
        {
          id: 'yearless',
          title: 'Heat',
          year: null,
          paths: ['Heat.mkv'],
          assignment: { revision: 7, library_item_ids: ['i1'] },
        },
      ],
    } as never)
    const wrapper = mount(MatchDialog, {
      props: { item: item({ collection_item_id: initialCopy }) },
    })
    await flushPromises()
    if (initialCopy === undefined) {
      expect(api.adminReviewSearch).toHaveBeenLastCalledWith({
        kind: 'movie',
        query: 'Heat',
        year: 1995,
        item: 'dated',
      })
      await wrapper.find('#match-copy').setValue('yearless')
      await flushPromises()
    }
    return wrapper
  }

  test.each([undefined, 'yearless'])(
    'a selected yearless copy searches without the representative year (initial copy %s)',
    async (initialCopy) => {
      const wrapper = await selectYearlessCopy(initialCopy)
      try {
        expect(api.adminReviewSearch).toHaveBeenLastCalledWith({
          kind: 'movie',
          query: 'Heat',
          year: null,
          item: 'yearless',
        })
        expect(wrapper.find('#match-title').text()).toBe('Match “Heat”')
      } finally {
        wrapper.unmount()
      }
    },
  )

  test.each([undefined, 'yearless'])(
    'a selected yearless copy initializes a new item without a year (initial copy %s)',
    async (initialCopy) => {
      const wrapper = await selectYearlessCopy(initialCopy)
      try {
        const creation = wrapper.find('details')
        creation.element.setAttribute('open', '')
        expect
          .soft((creation.find('input[type="number"]').element as HTMLInputElement).value)
          .toBe('')
        await creation.find('button').trigger('click')
        await flushPromises()
        expect(api.adminApplyMatch).toHaveBeenCalledWith(
          'yearless',
          expect.objectContaining({
            action: 'new',
            expected_revision: 7,
            new_item: expect.objectContaining({ title: 'Heat', year: null }),
          }),
        )
      } finally {
        wrapper.unmount()
      }
    },
  )

  test.each([undefined, 'copy-b'])(
    'identifies the host of identical collection paths (copy %s)',
    async (selectedCopy) => {
      vi.mocked(api.itemDetail).mockResolvedValue({
        copies: ['a', 'b'].map((suffix) => ({
          id: `copy-${suffix}`,
          module_id: `host-${suffix}`,
          collection_id: 'movies',
          title: 'Heat',
          year: 1995,
          paths: ['Heat (1995).mkv'],
          assignment: { revision: 4, library_item_ids: ['i1'] },
        })),
      } as never)
      const wrapper = mount(MatchDialog, {
        props: { item: item({ collection_item_id: selectedCopy }) },
      })
      await flushPromises()
      if (selectedCopy === undefined) {
        expect(wrapper.findAll('#match-copy option').map((option) => option.text())).toEqual([
          'host-a · movies · Heat (1995).mkv',
          'host-b · movies · Heat (1995).mkv',
        ])
        await wrapper.find('#match-copy').setValue('copy-b')
        await flushPromises()
      } else {
        expect(wrapper.text()).toContain('host-b · movies')
        expect(wrapper.text()).not.toContain('host-a')
      }
      await wrapper
        .findAll('button')
        .find((button) => button.text() === 'Reject current')!
        .trigger('click')
      expect(api.adminApplyMatch).toHaveBeenCalledWith(
        'copy-b',
        expect.objectContaining({ action: 'reject' }),
      )
      wrapper.unmount()
    },
  )

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
    expect(wrapper.text()).toContain('selected copy and all its file parts')
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
      expected_revision: 4,
      library_item_ids: null,
      new_item: null,
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

  test.each([false, true])(
    'a pending save keeps the selected copy locked until it settles (refused=%s)',
    async (refused) => {
      const pending = held(undefined)
      vi.mocked(api.adminApplyMatch).mockImplementation(async () => {
        await pending.promise
        if (refused) throw new ApiError(409, 'The assignment changed')
        return { library_item_ids: ['saved-work'], revision: 5 } as never
      })
      vi.mocked(api.itemDetail).mockResolvedValue({
        copies: ['copy-a', 'copy-b'].map((id) => ({
          id,
          title: id,
          year: null,
          paths: [],
          assignment: { revision: 4, library_item_ids: ['i1'] },
        })),
      } as never)
      const wrapper = mount(MatchDialog, { props: { item: item() } })
      await flushPromises()
      const copy = wrapper.find('#match-copy')
      const reject = wrapper.findAll('button').find((button) => button.text() === 'Reject current')!
      try {
        await reject.trigger('click')
        expect.soft((copy.element as HTMLSelectElement).disabled).toBe(true)
        expect(api.adminApplyMatch).toHaveBeenCalledWith(
          'copy-a',
          expect.objectContaining({ action: 'reject', expected_revision: 4 }),
        )
        // Enter can submit even when the Search button is disabled. Finishing
        // that search used to clear the shared busy flag while saving continued.
        await wrapper.find('#match-query').setValue('another title')
        await wrapper.find('form').trigger('submit')
        await flushPromises()
        expect.soft(api.adminReviewSearch).toHaveBeenCalledTimes(1)
        expect.soft((copy.element as HTMLSelectElement).disabled).toBe(true)
        expect.soft(reject.attributes('disabled')).toBeDefined()
        expect(wrapper.emitted('applied')).toBeUndefined()
        expect(wrapper.emitted('close')).toBeUndefined()
        pending.settle()
        await flushPromises()
        if (refused) {
          expect(wrapper.find('[role="alert"]').text()).toContain('The assignment changed')
          expect((copy.element as HTMLSelectElement).disabled).toBe(false)
          expect(reject.attributes('disabled')).toBeUndefined()
          expect(wrapper.emitted('close')).toBeUndefined()
          await copy.setValue('copy-b')
          await flushPromises()
          expect(api.adminReviewSearch).toHaveBeenLastCalledWith(
            expect.objectContaining({ item: 'copy-b' }),
          )
        } else {
          expect(wrapper.emitted('applied')).toEqual([[['saved-work']]])
          expect(wrapper.emitted('close')).toHaveLength(1)
        }
        expect(api.adminApplyMatch).toHaveBeenCalledTimes(1)
      } finally {
        wrapper.unmount()
      }
    },
  )

  test('copy changes still supersede a pending search without disturbing a later save', async () => {
    const oldSearch = held({ candidates: [candidate({ title: 'Old copy result' })] })
    const save = held({ library_item_ids: ['saved-work'], revision: 5 })
    vi.mocked(api.adminReviewSearch)
      .mockReturnValueOnce(oldSearch.promise as never)
      .mockResolvedValue({ candidates: [candidate({ title: 'New copy result' })] } as never)
    vi.mocked(api.adminApplyMatch).mockReturnValue(save.promise as never)
    vi.mocked(api.itemDetail).mockResolvedValue({
      copies: ['copy-a', 'copy-b'].map((id) => ({
        id,
        title: id,
        year: null,
        paths: [],
        assignment: { revision: 4, library_item_ids: ['i1'] },
      })),
    } as never)
    const wrapper = mount(MatchDialog, { props: { item: item() } })
    await flushPromises()
    try {
      const copy = wrapper.find('#match-copy')
      expect((copy.element as HTMLSelectElement).disabled).toBe(false)
      await copy.setValue('copy-b')
      await flushPromises()
      expect(wrapper.text()).toContain('New copy result')
      await wrapper.find('ul button').trigger('click')
      oldSearch.settle()
      await flushPromises()
      expect(wrapper.text()).not.toContain('Old copy result')
      expect((copy.element as HTMLSelectElement).value).toBe('copy-b')
      expect((copy.element as HTMLSelectElement).disabled).toBe(true)
      expect(wrapper.emitted('close')).toBeUndefined()
      expect(api.adminApplyMatch).toHaveBeenCalledWith(
        'copy-b',
        expect.objectContaining({
          candidate: expect.objectContaining({ title: 'New copy result' }),
        }),
      )
      save.settle()
      await flushPromises()
      expect(wrapper.emitted('applied')).toEqual([[['saved-work']]])
      expect(wrapper.emitted('close')).toHaveLength(1)
    } finally {
      wrapper.unmount()
    }
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
    await flushPromises()
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
    await flushPromises()
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
    await flushPromises()
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
  test.each([undefined, 'selected-copy'])(
    'confirmation names the selected copy’s record (initial copy %s)',
    async (initialCopy) => {
      vi.mocked(api.itemDetail).mockResolvedValue({
        copies: [
          {
            id: 'representative-copy',
            title: 'Episode 1',
            year: 2000,
            paths: ['show/S01E01.mkv'],
            match_confidence: 'weak',
            matched_title: 'Representative episode',
            matched_year: 2000,
            assignment: { revision: 3, library_item_ids: ['i1'] },
          },
          {
            id: 'selected-copy',
            title: 'Episode 1',
            year: 2000,
            paths: ['show/S01E01.mp4'],
            match_confidence: 'weak',
            matched_title: 'Selected episode',
            matched_year: 2001,
            assignment: { revision: 7, library_item_ids: ['i1'] },
          },
        ],
      } as never)
      const wrapper = mount(MatchDialog, {
        props: {
          item: item({
            kind: 'episode',
            collection_item_id: initialCopy,
            matched_title: 'Representative episode',
            year: 2000,
          }),
        },
      })
      await flushPromises()
      if (initialCopy === undefined) {
        expect(wrapper.text()).toMatch(/Representative episode\s+\(2000\)/)
        await wrapper.find('#match-copy').setValue('selected-copy')
        await flushPromises()
      }
      try {
        expect(wrapper.text()).toMatch(/Selected episode\s+\(2001\)/)
        expect(wrapper.text()).not.toContain('Representative episode')
        await wrapper
          .findAll('button')
          .find((button) => button.text() === 'Confirm current')!
          .trigger('click')
        await flushPromises()
        expect(api.adminApplyMatch).toHaveBeenCalledWith(
          'selected-copy',
          expect.objectContaining({ action: 'confirm', expected_revision: 7 }),
        )
      } finally {
        wrapper.unmount()
      }
    },
  )

  test('missing selected match fields cannot borrow the representative title or year', async () => {
    vi.mocked(api.itemDetail).mockResolvedValue({
      copies: [
        {
          id: 'selected-copy',
          title: 'Episode 1',
          year: 2000,
          paths: [],
          match_confidence: 'weak',
          matched_title: null,
          matched_year: null,
          assignment: { revision: 7, library_item_ids: ['i1'] },
        },
      ],
    } as never)
    const wrapper = mount(MatchDialog, {
      props: { item: item({ collection_item_id: 'selected-copy' }) },
    })
    await flushPromises()
    try {
      expect(wrapper.text()).toContain('Match title unavailable')
      expect(wrapper.text()).not.toContain('Heat 2 (fan edit)')
      expect(wrapper.text()).not.toContain('2022')
    } finally {
      wrapper.unmount()
    }
  })

  test.each(['auto', 'weak'])(
    'the selected copy controls confirmation when the library item confidence is %s',
    async (confidence) => {
      vi.mocked(api.itemDetail).mockResolvedValue({
        copies: [
          {
            id: 'certain',
            title: 'Heat',
            year: 1995,
            paths: [],
            match_confidence: 'auto',
            assignment: { revision: 3, library_item_ids: ['i1'] },
          },
          {
            id: 'uncertain',
            title: 'Heat',
            year: 1995,
            paths: [],
            match_confidence: 'weak',
            assignment: { revision: 4, library_item_ids: ['i1'] },
          },
          {
            id: 'unmatched',
            title: 'Heat',
            year: 1995,
            paths: [],
            match_confidence: null,
            assignment: { revision: 5, library_item_ids: ['i1'] },
          },
        ],
      } as never)
      const wrapper = mount(MatchDialog, {
        props: { item: item({ match_confidence: confidence }) },
      })
      await flushPromises()
      expect(wrapper.text()).not.toContain('Uncertain match')
      await wrapper.find('#match-copy').setValue('unmatched')
      await flushPromises()
      expect(wrapper.text()).not.toContain('Uncertain match')
      await wrapper.find('#match-copy').setValue('uncertain')
      await flushPromises()
      expect(wrapper.text()).toContain('Uncertain match')
      await wrapper
        .findAll('button')
        .find((b) => b.text() === 'Confirm current')!
        .trigger('click')
      await flushPromises()
      expect(api.adminApplyMatch).toHaveBeenCalledWith(
        'uncertain',
        expect.objectContaining({
          action: 'confirm',
          expected_revision: 4,
        }),
      )
      wrapper.unmount()
    },
  )

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
      expected_revision: 4,
      library_item_ids: null,
      new_item: null,
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
  test.each([false, true])(
    'Tab wraps around the visible controls with creation open=%s',
    async (expanded) => {
      vi.mocked(api.adminReviewSearch).mockResolvedValue({ candidates: [] } as never)
      const wrapper = await open({ kind: 'episode' })
      const details = wrapper.find('details')
      if (expanded) details.element.setAttribute('open', '')
      const first = wrapper.find('[aria-label="Close"]').element as HTMLElement
      const last = (expanded ? details.find('button') : details.find('summary'))
        .element as HTMLElement
      last.focus()
      const forward = new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true })
      last.dispatchEvent(forward)
      expect(forward.defaultPrevented).toBe(true)
      expect(document.activeElement).toBe(first)
      first.dispatchEvent(
        new KeyboardEvent('keydown', {
          key: 'Tab',
          shiftKey: true,
          bubbles: true,
          cancelable: true,
        }),
      )
      expect(document.activeElement).toBe(last)
      wrapper.unmount()
    },
  )

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

describe('local assignment pages', () => {
  const episode = (id: string, parent: string, title = 'Pilot') => ({
    id,
    kind: 'episode',
    title,
    parent_title: parent,
    season: 1,
    episode: 1,
    episode_end: null,
  })

  test('a first page of other kinds cannot hide eligible episodes on later pages', async () => {
    vi.mocked(api.listItems).mockImplementation(
      async (params) =>
        ({
          items:
            params?.offset === 200
              ? [episode('wanted', 'The target series')]
              : Array.from({ length: 200 }, (_, i) => ({
                  id: `movie-${i}`,
                  kind: 'movie',
                  title: 'Pilot',
                })),
          total: 201,
          offset: params?.offset ?? 0,
          limit: 200,
        }) as never,
    )
    const wrapper = await open({ kind: 'episode' })
    await wrapper
      .findAll('button')
      .find((button) => button.text() === 'Load more library items')!
      .trigger('click')
    await flushPromises()
    expect(api.listItems).toHaveBeenLastCalledWith({ q: 'Heat', limit: 200, offset: 200 })
    expect(wrapper.text()).toContain('The target series · S01E01 · Pilot')
    await wrapper.find('input[type="checkbox"]').setValue(true)
    await wrapper
      .findAll('button')
      .find((button) => button.text() === 'Assign selected episodes')!
      .trigger('click')
    await flushPromises()
    expect(api.adminApplyMatch).toHaveBeenCalledWith(
      'i1',
      expect.objectContaining({ action: 'assign', library_item_ids: ['wanted'] }),
    )
    wrapper.unmount()
  })

  test('episode choices retain playback order across pages and title searches', async () => {
    vi.mocked(api.listItems).mockImplementation(
      async (params) =>
        ({
          items:
            params?.q === 'Finale'
              ? [episode('finale', 'Chosen series', 'Finale')]
              : params?.offset === 200
                ? [episode('later', 'Chosen series')]
                : Array.from({ length: 200 }, (_, i) => episode(`episode-${i}`, `Series ${i}`)),
          total: params?.q === 'Finale' ? 1 : 201,
          offset: params?.offset ?? 0,
          limit: 200,
        }) as never,
    )
    const wrapper = await open({ kind: 'episode' })
    await wrapper.find('input[type="checkbox"]').setValue(true)
    await wrapper
      .findAll('button')
      .find((button) => button.text() === 'Load more library items')!
      .trigger('click')
    await flushPromises()
    await wrapper.findAll('input[type="checkbox"]').at(-1)!.setValue(true)
    await wrapper.find('#match-query').setValue('Finale')
    await wrapper.find('form').trigger('submit')
    await flushPromises()
    await wrapper.find('input[type="checkbox"]').setValue(true)
    expect(wrapper.findAll('ol li').map((entry) => entry.text())).toEqual([
      expect.stringContaining('Series 0 · S01E01 · Pilot'),
      expect.stringContaining('Chosen series · S01E01 · Pilot'),
      expect.stringContaining('Chosen series · S01E01 · Finale'),
    ])
    await wrapper
      .findAll('button')
      .find((button) => button.text() === 'Assign selected episodes')!
      .trigger('click')
    await flushPromises()
    expect(api.adminApplyMatch).toHaveBeenCalledWith(
      'i1',
      expect.objectContaining({ library_item_ids: ['episode-0', 'later', 'finale'] }),
    )
    wrapper.unmount()
  })

  test('a page arriving after a new query cannot append old candidates', async () => {
    const old = held({ items: [episode('old', 'Old series')], offset: 1, limit: 200, total: 2 })
    vi.mocked(api.listItems).mockImplementation(async (params) => {
      if (params?.offset === 1) return old.promise as never
      return {
        items: [
          episode(
            params?.q === 'New' ? 'new' : 'first',
            params?.q === 'New' ? 'New series' : 'First series',
          ),
        ],
        offset: 0,
        limit: 200,
        total: params?.q === 'New' ? 1 : 2,
      } as never
    })
    const wrapper = await open({ kind: 'episode' })
    await wrapper
      .findAll('button')
      .find((button) => button.text() === 'Load more library items')!
      .trigger('click')
    await wrapper.find('#match-query').setValue('New')
    await wrapper.find('form').trigger('submit')
    await flushPromises()
    old.settle()
    await flushPromises()
    expect(wrapper.text()).toContain('New series')
    expect(wrapper.text()).not.toContain('Old series')
    wrapper.unmount()
  })
})

test('a removed explicit copy cannot fall back to another copy', async () => {
  const wrapper = await open({ collection_item_id: 'removed' })
  expect(wrapper.text()).toContain('This source is no longer available')
  expect(wrapper.find('#match-copy').exists()).toBe(false)
  expect(api.adminReviewSearch).not.toHaveBeenCalled()
  const reject = wrapper.findAll('button').find((b) => b.text() === 'Reject current')!
  expect(reject.attributes('disabled')).toBeDefined()
  wrapper.unmount()
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
