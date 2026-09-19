import { onlineManager, QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { flushPromises, mount } from '@vue/test-utils'
import { defineComponent, h } from 'vue'
import { beforeEach, expect, test, vi } from 'vitest'

vi.mock('../src/api/generated/kahawai.ts', () => ({ libraries: vi.fn() }))
vi.mock('../src/api/playback.ts', () => ({ queryPlaybackItem: vi.fn() }))
import { libraries } from '../src/api/generated/kahawai.ts'
import { queryPlaybackItem } from '../src/api/playback.ts'
import { librariesQuery, useLibraries } from '../src/composables/catalogue.ts'
import { playbackItemQuery } from '../src/composables/item.ts'

beforeEach(() => vi.resetAllMocks())

test('shell, page and imperative player reads share one in-flight enumeration and shape', async () => {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  const rows = [{ id: 'films', name: 'Films', media_type: 'movies', collection_ids: [] }]
  let finish!: (value: typeof rows) => void
  vi.mocked(libraries).mockReturnValue(
    new Promise((resolve) => {
      finish = resolve
    }),
  )
  const Consumer = defineComponent({
    setup() {
      const query = useLibraries()
      return () => h('div', query.data.value?.map((l) => l.name).join(','))
    },
  })
  const wrapper = mount(defineComponent({ render: () => h('main', [h(Consumer), h(Consumer)]) }), {
    global: { plugins: [[VueQueryPlugin, { queryClient: client }]] },
  })
  const player = client.fetchQuery(librariesQuery)
  expect(libraries).toHaveBeenCalledTimes(1)
  finish(rows)
  expect(await player).toEqual(rows)
  await flushPromises()
  expect(wrapper.findAll('div').map((w) => w.text())).toEqual(['Films', 'Films'])
  client.setQueryData(librariesQuery.queryKey, [{ ...rows[0]!, name: 'Renamed' }])
  await flushPromises()
  expect(wrapper.findAll('div').map((w) => w.text())).toEqual(['Renamed', 'Renamed'])
  expect(client.getQueryCache().getAll()).toHaveLength(1)
  wrapper.unmount()
  client.clear()
})

test('later mounts and player reads reuse libraries; mutations and reconnects refresh them', async () => {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  vi.mocked(libraries).mockResolvedValue([
    { id: 'films', name: 'Films', media_type: 'movies', collection_ids: [] },
  ])
  const Consumer = defineComponent({
    setup() {
      const query = useLibraries()
      return () => h('div', query.data.value?.map((l) => l.name).join(','))
    },
  })
  const consumer = () =>
    mount(Consumer, {
      global: { plugins: [[VueQueryPlugin, { queryClient: client }]] },
    })
  const shell = consumer()
  await flushPromises()
  const page = consumer()
  try {
    await flushPromises()
    await client.fetchQuery(librariesQuery)
    expect(libraries).toHaveBeenCalledTimes(1)
    expect(page.text()).toBe('Films')
    vi.mocked(libraries).mockResolvedValue([
      { id: 'films', name: 'Renamed', media_type: 'movies', collection_ids: [] },
    ])
    await client.invalidateQueries({ queryKey: librariesQuery.queryKey })
    await flushPromises()
    expect(libraries).toHaveBeenCalledTimes(2)
    expect(shell.text()).toBe('Renamed')
    expect(page.text()).toBe('Renamed')
    onlineManager.setOnline(false)
    onlineManager.setOnline(true)
    await flushPromises()
    expect(libraries).toHaveBeenCalledTimes(3)
  } finally {
    onlineManager.setOnline(true)
    page.unmount()
    shell.unmount()
    client.clear()
  }
})

test('source queries cannot reuse another preference, media type or rendition answer', async () => {
  const client = new QueryClient()
  vi.mocked(queryPlaybackItem).mockResolvedValue({ id: 'item' } as never)
  const options = playbackItemQuery('item', 'films', [], 'movies', 1, 'entry-a')
  await client.fetchQuery({ ...options, staleTime: Infinity })
  await client.fetchQuery({ ...options, staleTime: Infinity })
  expect(queryPlaybackItem).toHaveBeenCalledTimes(1)
  for (const changed of [
    playbackItemQuery(
      'item',
      'films',
      [{ scope: '', key: 'audio.movies', value: 'jpn' }],
      'movies',
      1,
      'entry-a',
    ),
    playbackItemQuery('item', 'films', [], 'anime', 1, 'entry-a'),
    playbackItemQuery('item', 'films', [], 'movies', 1, 'entry-b'),
  ])
    await client.fetchQuery({ ...changed, staleTime: Infinity })
  expect(queryPlaybackItem).toHaveBeenCalledTimes(4)
  client.clear()
})
