import { QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import Admin from '../src/views/Admin.vue'
import * as api from '../src/api/generated/kahawai.ts'
import type { CatalogueCollection } from '../src/api/generated/model/catalogueCollection.ts'
import type { CatalogueLibrary } from '../src/api/generated/model/catalogueLibrary.ts'
import { ApiError } from '../src/api/errors.ts'

vi.mock('../src/api/generated/kahawai.ts')
vi.mock('../src/api/session.ts', () => ({ whoAmI: () => ({ username: 'boss', admin: true }) }))
const collection = (
  id: string,
  overrides: Partial<CatalogueCollection> = {},
): CatalogueCollection => ({
  id,
  remote_id: 'movies',
  mediahost_id: id === 'a' ? 'local' : 'nas',
  media_type: 'movies',
  connected: true,
  scanning: false,
  snapshot: false,
  file_count: 42,
  epoch: 'epoch',
  version: 42,
  roots: [{ id: `root-${id}`, token: id, path: `/media/${id}`, active: true }],
  ...overrides,
})
let rows: CatalogueLibrary[]
let collections: CatalogueCollection[]
let wrapper: VueWrapper
let client: QueryClient
beforeEach(() => {
  rows = [{ id: 'films', name: 'Films', media_type: 'movies', collection_ids: ['a'] }]
  collections = [
    collection('a'),
    collection('b', { connected: false, scanning: true }),
    collection('music', { media_type: 'music' }),
  ]
  vi.mocked(api.libraries).mockImplementation(async () => structuredClone(rows))
  vi.mocked(api.collections).mockImplementation(async () => structuredClone(collections))
  vi.mocked(api.adminEnrollments).mockResolvedValue({ pending: [] })
  vi.mocked(api.adminSatellites).mockResolvedValue({
    satellites: [{ module_id: 'nas', name: 'Attic', cert_fingerprint: 'cert' }],
  } as never)
  vi.mocked(api.adminUsers).mockResolvedValue({ users: [] })
  vi.mocked(api.adminSessions).mockRejectedValue(new ApiError(501, 'unavailable'))
  vi.mocked(api.createLibrary).mockImplementation(async (body) => {
    const row = { ...body, id: 'created', collection_ids: body.collection_ids ?? [] }
    rows.push(row)
    return structuredClone(row)
  })
  vi.mocked(api.setCollections).mockImplementation(async (id, body) => {
    rows = rows.map((row) =>
      row.id === id ? { ...row, collection_ids: [...body.collection_ids] } : row,
    )
  })
  vi.mocked(api.deleteLibrary).mockImplementation(async (id) => {
    rows = rows.filter((row) => row.id !== id)
  })
})
afterEach(() => {
  wrapper?.unmount()
  client?.clear()
  vi.resetAllMocks()
})
async function open() {
  client = new QueryClient({ defaultOptions: { queries: { retry: false, gcTime: 0 } } })
  wrapper = mount(Admin, { global: { plugins: [[VueQueryPlugin, { queryClient: client }]] } })
  await flushPromises()
  await wrapper.get('#tab-libraries').trigger('click')
  await flushPromises()
}
async function press(text: string) {
  const button = wrapper.findAll('button').find((button) => button.text() === text)!
  if (button.attributes('type') === 'submit') await forms()[1]!.trigger('submit')
  else await button.trigger('click')
  await flushPromises()
}

async function edit() {
  await press('Edit collections')
}
const forms = () => wrapper.findAll('form')
function held<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => {
    resolve = done
  })
  return { promise, resolve }
}

test('reads only the active catalogue APIs and names local/offline collections with their roots', async () => {
  await open()
  const positions = () =>
    forms()[0]!
      .findAll('input[type="checkbox"]')
      .map((x) => x.attributes('value'))
  await forms()[0]!.get('input[value="b"]').setValue(true)
  expect(positions()).toEqual(['a', 'b'])
  await forms()[0]!.get('input[value="a"]').setValue(true)
  await forms()[0]!.get('input[value="a"]').setValue(false)
  await client.invalidateQueries({ queryKey: ['admin', 'collections'] })
  await flushPromises()
  expect(positions()).toEqual(['a', 'b'])
  expect(api.adminSessions).not.toHaveBeenCalled()
  expect(wrapper.text()).toContain('This hub/movies')
  expect(wrapper.text()).toContain('Attic/movies')
  expect(wrapper.text()).toContain('42 files')
  expect(wrapper.text()).toContain('offline')
  expect(wrapper.text()).toContain('scanning')
  expect(wrapper.text()).toContain('/media/b')
  expect(
    forms()[0]!
      .findAll('input[type="checkbox"]')
      .map((x) => x.attributes('value')),
  ).toEqual(['a', 'b'])
})

test('creates with the selected collections; switching media type clears incompatible selections', async () => {
  await open()
  await wrapper.get('#new-library').setValue('  Cinema  ')
  await forms()[0]!.get('input[value="a"]').setValue(true)
  await wrapper.get('#new-library-type').setValue('music')
  expect(forms()[0]!.find('input[value="a"]').exists()).toBe(false)
  await forms()[0]!.get('input[value="music"]').setValue(true)
  await forms()[0]!.trigger('submit')
  await flushPromises()
  expect(api.createLibrary).toHaveBeenCalledWith({
    name: 'Cinema',
    media_type: 'music',
    collection_ids: ['music'],
  })
  expect((wrapper.get('#new-library').element as HTMLInputElement).value).toBe('')
  expect(rows[1]?.collection_ids).toEqual(['music'])
})

test('saves the whole ordered membership and leaves cancel without a write', async () => {
  await open()
  await edit()
  await forms()[1]!.get('input[value="b"]').setValue(true)
  await forms()[1]!.get('[aria-label="Move Attic/movies earlier"]').trigger('click')
  expect(
    forms()[1]!
      .findAll('input[type="checkbox"]')
      .map((x) => x.attributes('value')),
  ).toEqual(['b', 'a'])
  expect(api.setCollections).not.toHaveBeenCalled()
  await press('Save collections')
  expect(api.setCollections).toHaveBeenCalledWith('films', { collection_ids: ['b', 'a'] })
  await edit()
  await forms()[1]!.get('input[value="b"]').setValue(false)
  await press('Cancel')
  expect(api.setCollections).toHaveBeenCalledTimes(1)
  expect(rows[0]!.collection_ids).toEqual(['b', 'a'])
})

test('detaching the last collection saves an empty library', async () => {
  await open()
  await edit()
  await forms()[1]!.get('input[value="a"]').setValue(false)
  await press('Save collections')
  expect(api.setCollections).toHaveBeenCalledWith('films', { collection_ids: [] })
  expect(wrapper.text()).toContain('No collections assigned.')
})

test('a failed save retains the draft through a poll and supports retry', async () => {
  await open()
  await edit()
  await forms()[1]!.get('input[value="b"]').setValue(true)
  vi.mocked(api.setCollections).mockRejectedValueOnce(new ApiError(409, 'membership changed'))
  await press('Save collections')
  await client.invalidateQueries({ queryKey: ['admin'] })
  await flushPromises()
  expect(wrapper.get('[role="alert"]').text()).toContain('membership changed')
  expect((forms()[1]!.get('input[value="b"]').element as HTMLInputElement).checked).toBe(true)
  await press('Save collections')
  expect(rows[0]!.collection_ids).toEqual(['a', 'b'])
  expect(wrapper.get('[role="alert"]').text()).toBe('')
})

test('prevents duplicate submissions while a write is pending', async () => {
  await open()
  await edit()
  const pending = held<void>()
  vi.mocked(api.setCollections).mockReturnValueOnce(pending.promise)
  await forms()[1]!.trigger('submit')
  await forms()[1]!.trigger('submit')
  expect(api.setCollections).toHaveBeenCalledTimes(1)
  expect(
    wrapper
      .findAll('button')
      .find((b) => b.text() === 'Saving…')!
      .attributes('disabled'),
  ).toBeDefined()
  pending.resolve()
  await flushPromises()
})

test('blocks saving a disappeared collection until it is removed from the draft', async () => {
  await open()
  await edit()
  collections = collections.filter((c) => c.id !== 'a')
  await client.invalidateQueries({ queryKey: ['admin', 'collections'] })
  await flushPromises()
  expect(wrapper.text()).toContain('collection no longer available')
  expect(
    wrapper
      .findAll('button')
      .find((b) => b.text() === 'Save collections')!
      .attributes('disabled'),
  ).toBeDefined()
  await forms()[1]!.get('input[value="a"]').setValue(false)
  await press('Save collections')
  expect(api.setCollections).toHaveBeenCalledWith('films', { collection_ids: [] })
})

test('read failures never present an empty catalogue as fact or allow overwriting membership', async () => {
  vi.mocked(api.collections).mockRejectedValue(new ApiError(503, 'offline'))
  vi.mocked(api.libraries).mockRejectedValue(new ApiError(503, 'offline'))
  await open()
  expect(wrapper.text()).toContain('libraries could not be read')
  expect(wrapper.text()).not.toContain('No libraries yet')
  expect(wrapper.text()).not.toContain('No collections of this type')
  expect(
    wrapper
      .findAll('button')
      .find((b) => b.text() === 'Create')!
      .attributes('disabled'),
  ).toBeDefined()
})

test('deleting requires confirmation and never deletes imported collections', async () => {
  await open()
  await press('Delete')
  expect(api.deleteLibrary).not.toHaveBeenCalled()
  await press('Really delete?')
  expect(api.deleteLibrary).toHaveBeenCalledWith('films')
  expect(wrapper.text()).toContain('No libraries yet')
  expect(collections).toHaveLength(3)
})

test('rescans committed membership with explicit normal and deep intent', async () => {
  vi.mocked(api.refreshLibrary).mockResolvedValue({ asked: 1, offline: 0, unsupported: 0 })
  await open()
  await press('Rescan')
  expect(api.refreshLibrary).toHaveBeenLastCalledWith(rows[0]!.id, { deep: false })
  await press('Deep rescan')
  expect(api.refreshLibrary).toHaveBeenLastCalledWith(rows[0]!.id, { deep: true })
  expect(api.setCollections).not.toHaveBeenCalled()
})
