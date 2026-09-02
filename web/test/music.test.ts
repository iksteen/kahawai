import { flushPromises, mount } from '@vue/test-utils'
import { defineComponent, h, nextTick, ref } from 'vue'
import { afterEach, expect, test, vi } from 'vitest'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  artistAlbums: vi.fn(),
  listArtists: vi.fn(),
}))

const { artistAlbums, listArtists } = await import('../src/api/generated/kahawai.ts')
const { useArtistAlbums, useArtists } = await import('../src/composables/music.ts')

afterEach(() => vi.resetAllMocks())

test('loads artist rows by the chunks requested by the virtual grid', async () => {
  vi.mocked(listArtists).mockImplementation(async (params) => {
    const offset = params?.offset ?? 0
    return {
      artists: [
        { key: `artist-${offset}`, name: `Artist ${offset}`, album_count: 1, art_version: null },
      ],
      total: 201,
      limit: 100,
      offset,
    }
  })
  let artists!: ReturnType<typeof useArtists>
  mount(
    defineComponent({
      setup() {
        artists = useArtists(ref('music'), ref(''), ref('name'))
        return () => h('div')
      },
    }),
  )
  await flushPromises()

  artists.need([1])
  await flushPromises()
  expect(listArtists).toHaveBeenCalledWith(expect.objectContaining({ limit: 100, offset: 100 }))
  expect(artists.loaded.value.get(100)?.key).toBe('artist-100')
})

test('keeps a fresh later chunk when it lands before replacement page zero', async () => {
  const pending = new Map<number, (answer: Awaited<ReturnType<typeof listArtists>>) => void>()
  vi.mocked(listArtists).mockImplementation(async (params) => {
    if (params.sort === 'name') {
      return {
        artists: [{ key: 'old', name: 'Old', album_count: 1, art_version: null }],
        total: 101,
        limit: 100,
        offset: 0,
      }
    }
    return new Promise((resolve) => pending.set(params.offset ?? 0, resolve))
  })
  const sort = ref('name')
  let artists!: ReturnType<typeof useArtists>
  mount(
    defineComponent({
      setup() {
        artists = useArtists(ref('music'), ref(''), sort)
        return () => h('div')
      },
    }),
  )
  await flushPromises()

  sort.value = '-name'
  await nextTick()
  artists.need([1])
  pending.get(100)!({
    artists: [{ key: 'new-100', name: 'New 100', album_count: 1, art_version: null }],
    total: 101,
    limit: 100,
    offset: 100,
  })
  await flushPromises()
  pending.get(0)!({
    artists: [{ key: 'new-0', name: 'New 0', album_count: 1, art_version: null }],
    total: 101,
    limit: 100,
    offset: 0,
  })
  await flushPromises()

  expect(artists.loaded.value.get(0)?.key).toBe('new-0')
  expect(artists.loaded.value.get(100)?.key).toBe('new-100')
})

test('clears another library’s clickable artists while the new route loads', async () => {
  let landSecond!: (answer: Awaited<ReturnType<typeof listArtists>>) => void
  vi.mocked(listArtists).mockImplementation(async (params) => {
    if (params.library === 'first') {
      return {
        artists: [{ key: 'old-artist', name: 'Old Artist', album_count: 1, art_version: null }],
        total: 1,
        limit: 100,
        offset: 0,
      }
    }
    return new Promise((resolve) => (landSecond = resolve))
  })
  const library = ref('first')
  let artists!: ReturnType<typeof useArtists>
  mount(
    defineComponent({
      setup() {
        artists = useArtists(library, ref(''), ref('name'))
        return () => h('div')
      },
    }),
  )
  await flushPromises()
  expect(artists.loaded.value.get(0)?.key).toBe('old-artist')

  library.value = 'second'
  await nextTick()
  expect(artists.total.value).toBeNull()
  expect(artists.loaded.value.size).toBe(0)

  landSecond({
    artists: [{ key: 'new-artist', name: 'New Artist', album_count: 1, art_version: null }],
    total: 1,
    limit: 100,
    offset: 0,
  })
  await flushPromises()
  expect(artists.loaded.value.get(0)?.key).toBe('new-artist')
})

test('clears another artist’s clickable albums while the new route loads', async () => {
  let landSecond!: (answer: Awaited<ReturnType<typeof artistAlbums>>) => void
  vi.mocked(artistAlbums).mockImplementation(async (key) => {
    if (key === 'first') {
      return {
        artist: { key, name: 'First', album_count: 1, art_version: null },
        albums: [{ id: 'old-album', title: 'Old Album' } as never],
        total: 1,
        limit: 100,
        offset: 0,
      }
    }
    return new Promise((resolve) => (landSecond = resolve))
  })
  const key = ref('first')
  let albums!: ReturnType<typeof useArtistAlbums>
  mount(
    defineComponent({
      setup() {
        albums = useArtistAlbums(ref('music'), key, ref(''), ref('year'))
        return () => h('div')
      },
    }),
  )
  await flushPromises()
  expect(albums.loaded.value.get(0)?.id).toBe('old-album')

  key.value = 'second'
  await nextTick()
  expect(albums.artist.value).toBeNull()
  expect(albums.total.value).toBeNull()
  expect(albums.loaded.value.size).toBe(0)

  landSecond({
    artist: { key: 'second', name: 'Second', album_count: 1, art_version: null },
    albums: [{ id: 'new-album', title: 'New Album' } as never],
    total: 1,
    limit: 100,
    offset: 0,
  })
  await flushPromises()
  expect(albums.loaded.value.get(0)?.id).toBe('new-album')
})
