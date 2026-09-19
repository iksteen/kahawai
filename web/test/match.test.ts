import { QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import type { ItemSummary } from '../src/api/catalogue-model.ts'
import type { EnrichmentDetail } from '../src/api/generated/model/enrichmentDetail.ts'
import MatchDialog from '../src/components/MatchDialog.vue'
import Card from '../src/components/Card.vue'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  adminProviders: vi.fn(),
  enrichmentDetail: vi.fn(),
  enrichmentIdentities: vi.fn(),
  getEnrichmentArtworkUrl: (id: string, params: { record_id?: string }) =>
    `/api/v1/catalogue/collection-items/${id}/artwork?${new URLSearchParams(params)}`,
  enrichmentCorrect: vi.fn(),
  enrichmentSearch: vi.fn(),
  item: vi.fn(),
  getCatalogueArtworkUrl: (library: string, id: string) =>
    `/api/v1/catalogue/libraries/${library}/items/${id}/artwork`,
}))
const api = await import('./api-fixture.ts')
const record = {
  provider: 'tmdb',
  external_id: '949',
  namespace: 'movie',
  language: 'en',
  media_type: 'movies' as const,
  title: 'Heat',
  year: 1995,
  description: { overview: 'A heist film.' },
}
const snapshot = (input: Partial<EnrichmentDetail['input']> = {}): EnrichmentDetail => ({
  input: {
    item_id: 'copy-1',
    library_item_id: 'work-1',
    collection_id: 'collection',
    mediahost_id: 'silence',
    remote_id: 'movies',
    media_type: 'movies',
    title: 'Heat',
    year: null,
    revision: 7,
    manual: false,
    selected: null,
    links: [],
    sources: [
      { file_id: 'file', root_token: 'films', root_path: '/films', path: 'Heat.mkv', media: {} },
    ],
    ...input,
  },
  candidates: [{ id: 'record', record, strength: 0, rejected: false }],
  metadata: { description: record.description, provenance: {} },
  entries: [],
})
let wrappers: ReturnType<typeof mount>[] = []
let clients: QueryClient[] = []
function dialog(direct = true) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  clients.push(client)
  const wrapper = mount(MatchDialog, {
    attachTo: document.body,
    props: {
      item: {
        id: 'work-1',
        title: 'Wrong display title',
        library_id: 'library',
        ...(direct ? { collection_item_id: 'copy-1' } : {}),
      },
    },
    global: { plugins: [[VueQueryPlugin, { queryClient: client }]] },
  })
  wrappers.push(wrapper)
  return wrapper
}
function button(wrapper: ReturnType<typeof mount>, text: string) {
  return wrapper.findAll('button').find((b) => b.text() === text)!
}
beforeEach(() => {
  vi.resetAllMocks()
  vi.mocked(api.enrichmentDetail).mockResolvedValue(snapshot())
  vi.mocked(api.enrichmentCorrect).mockResolvedValue({ ok: true })
  vi.mocked(api.enrichmentSearch).mockResolvedValue({
    detail: snapshot(),
    identities: [],
    errors: {},
  })
  vi.mocked(api.enrichmentIdentities).mockResolvedValue([])
})
afterEach(() => {
  wrappers.forEach((w) => w.unmount())
  clients.forEach((c) => c.clear())
  wrappers = []
  clients = []
  document.body.innerHTML = ''
})

describe('mediadb matching', () => {
  test('opens the copy identity and stored candidates without a provider request', async () => {
    const w = dialog()
    await flushPromises()
    expect(w.get('h2').text()).toBe('Match “Heat”')
    expect(w.text()).toContain('silence · movies')
    expect(w.text()).toContain('Heat.mkv')
    expect(w.get('input').element.value).toBe('Heat')
    expect(api.item).not.toHaveBeenCalled()
    expect(api.enrichmentSearch).not.toHaveBeenCalled()
    expect(w.get('ul.grid').text()).toContain('1995 · tmdb')
  })
  test('library cards choose between physical copies using catalogue detail', async () => {
    vi.mocked(api.item).mockResolvedValue({
      copies: [
        { id: 'copy-1', title: 'Heat', paths: ['Heat.mkv'] },
        { id: 'copy-2', title: 'Heat', paths: ['Heat.mp4'] },
      ],
    } as Awaited<ReturnType<typeof api.item>>)
    const w = dialog(false)
    await flushPromises()
    expect(api.item).toHaveBeenCalledWith('library', 'work-1')
    await w.get('#match-copy').setValue('copy-2')
    await flushPromises()
    expect(api.enrichmentDetail).toHaveBeenLastCalledWith('copy-2')
  })
  test('search refreshes the revision and picking emits the resulting stable library ID', async () => {
    const w = dialog()
    await flushPromises()
    vi.mocked(api.enrichmentSearch).mockResolvedValue({
      detail: snapshot({ revision: 8 }),
      identities: [],
      errors: {},
    })
    await w.get('input').setValue('Heat 1995')
    await w.get('form').trigger('submit')
    await flushPromises()
    expect(api.enrichmentSearch).toHaveBeenCalledExactlyOnceWith('copy-1', {
      revision: 7,
      query: 'Heat 1995',
    })
    expect(api.adminProviders).not.toHaveBeenCalled()
    expect(api.enrichmentDetail).toHaveBeenCalledTimes(1)
    expect(api.enrichmentIdentities).toHaveBeenCalledTimes(1)
    vi.mocked(api.enrichmentDetail).mockResolvedValue(
      snapshot({ revision: 9, library_item_id: 'work-2' }),
    )
    await w.get('ul.grid button').trigger('click')
    await flushPromises()
    expect(api.enrichmentCorrect).toHaveBeenCalledWith('copy-1', {
      revision: 8,
      action: 'pick',
      record_id: 'record',
    })
    expect(w.emitted('applied')).toEqual([[['work-2']]])
    expect(w.emitted('close')).toHaveLength(1)
  })
  test('failed search keeps the current match, query and a reload action', async () => {
    const w = dialog()
    await flushPromises()
    vi.mocked(api.enrichmentSearch).mockRejectedValue(new Error('Provider unavailable'))
    await w.get('input').setValue('Another title')
    await w.get('form').trigger('submit')
    await flushPromises()
    expect(w.get('[role=alert]').text()).toContain('Provider unavailable')
    expect(w.get('input').element.value).toBe('Another title')
    expect(w.text()).toContain('Uncertain match: Heat')
    expect(button(w, 'Search').attributes('disabled')).toBeUndefined()
    expect(button(w, 'Reload matches').exists()).toBe(true)
  })
  test('provider failure preserves results from another configured provider', async () => {
    const w = dialog()
    await flushPromises()
    vi.mocked(api.enrichmentSearch).mockResolvedValue({
      detail: {
        ...snapshot(),
        candidates: [
          { id: 'tv', record: { ...record, provider: 'tvdb' }, strength: 0, rejected: false },
        ],
      },
      identities: [{ id: 'existing', title: 'Existing Heat', year: 1995 }],
      errors: { tmdb: 'Unavailable' },
    })
    await w.get('form').trigger('submit')
    await flushPromises()
    expect(api.enrichmentSearch).toHaveBeenCalledTimes(1)
    expect(w.text()).toContain('Existing Heat')
    expect(w.get('ul.grid').text()).toContain('tvdb')
    expect(w.get('[role=alert]').text()).toContain('tmdb: Unavailable')
  })
  test('revision conflicts do not close or silently repeat the correction', async () => {
    const w = dialog()
    await flushPromises()
    vi.mocked(api.enrichmentCorrect).mockRejectedValue(new Error('Changed since review'))
    await button(w, 'Confirm current').trigger('click')
    await flushPromises()
    expect(w.get('[role=alert]').text()).toContain('Changed since review')
    expect(w.emitted('close')).toBeUndefined()
    vi.mocked(api.enrichmentDetail).mockResolvedValue(snapshot({ revision: 10 }))
    await button(w, 'Reload matches').trigger('click')
    await flushPromises()
    expect(api.enrichmentCorrect).toHaveBeenCalledTimes(1)
    expect(w.find('[role=alert]').exists()).toBe(false)
  })
  test('automatic matching clears a manual selection on this copy', async () => {
    vi.mocked(api.enrichmentDetail).mockResolvedValue(snapshot({ manual: true }))
    const w = dialog()
    await flushPromises()
    await button(w, 'Use automatic matching').trigger('click')
    await flushPromises()
    expect(api.enrichmentCorrect).toHaveBeenCalledWith('copy-1', {
      revision: 7,
      action: 'clear',
      record_id: null,
    })
  })
  test('music uses its own identity provider', async () => {
    vi.mocked(api.enrichmentDetail).mockResolvedValue(snapshot({ media_type: 'music' }))
    const w = dialog()
    await flushPromises()
    expect(w.find('#match-provider').exists()).toBe(false)
    await w.get('form').trigger('submit')
    await flushPromises()
    expect(api.enrichmentSearch).toHaveBeenCalledExactlyOnceWith('copy-1', {
      revision: 7,
      query: 'Heat',
    })
  })
  test('failed initial reads are errors, not empty results', async () => {
    vi.mocked(api.enrichmentDetail).mockRejectedValue(new Error('Source disappeared'))
    const w = dialog()
    await flushPromises()
    expect(w.get('[role=alert]').text()).toContain('Source disappeared')
    expect(w.text()).not.toContain('No candidates yet')
  })
  test('existing work selection uses the correction API without manual creation', async () => {
    vi.mocked(api.enrichmentIdentities).mockResolvedValue([
      { id: 'saved', title: 'Heat', year: 1995 },
    ])
    const w = dialog()
    await flushPromises()
    await button(w, 'Heat · 1995').trigger('click')
    await flushPromises()
    expect(api.enrichmentCorrect).toHaveBeenLastCalledWith('copy-1', {
      revision: 7,
      action: 'assign',
      record_id: null,
      library_item_id: 'saved',
    })
    expect(w.text()).not.toContain('Create a library item')
    expect(w.find('input[type=number]').exists()).toBe(false)
  })
  test('artwork uses same-origin images and falls back on load failure', async () => {
    vi.mocked(api.enrichmentDetail).mockResolvedValue(snapshot({ selected: ['record', record] }))
    const w = dialog()
    await flushPromises()
    expect(w.get('img').attributes('src')).toBe(
      '/api/v1/catalogue/collection-items/copy-1/artwork?',
    )
    await w.get('img').trigger('error')
    expect(w.find('img').exists()).toBe(false)
    expect(w.find('.ghost-art').exists()).toBe(true)
  })
  test('closing during a request cannot emit a late navigation', async () => {
    let finish!: () => void
    vi.mocked(api.enrichmentCorrect).mockImplementation(
      () =>
        new Promise((resolve) => {
          finish = () => resolve({ ok: true })
        }),
    )
    const w = dialog()
    await flushPromises()
    await button(w, 'Confirm current').trigger('click')
    w.unmount()
    wrappers = []
    finish()
    await flushPromises()
    expect(w.emitted('applied')).toBeUndefined()
  })
})

describe('dialog keyboard', () => {
  test('Escape works outside the field and focus returns to the opener', async () => {
    const opener = document.createElement('button')
    document.body.append(opener)
    opener.focus()
    const w = dialog()
    await flushPromises()
    expect(document.activeElement).toBe(w.get('input').element)
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))
    expect(w.emitted('close')).toHaveLength(1)
    w.unmount()
    wrappers = []
    expect(document.activeElement).toBe(opener)
  })
  test('Tab wraps and skips controls inside closed disclosures', async () => {
    const w = dialog()
    await flushPromises()
    const summary = w.get('ul.grid button').element as HTMLElement
    summary.focus()
    const event = new KeyboardEvent('keydown', { key: 'Tab', cancelable: true })
    window.dispatchEvent(event)
    expect(event.defaultPrevented).toBe(true)
    expect(document.activeElement).toBe(button(w, '✕').element)
    window.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Tab', shiftKey: true, cancelable: true }),
    )
    expect(document.activeElement).toBe(summary)
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
    }) as unknown as ItemSummary & { played: boolean }

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
