import { onScopeDispose, type Ref, ref, shallowRef, watch } from 'vue'

import { artistAlbums, listArtists } from '../api/generated/kahawai.ts'
import type { ArtistSummary } from '../api/generated/model/artistSummary.ts'
import type { ItemRowI64 } from '../api/generated/model/itemRowI64.ts'
import { CHUNK } from '../domain/virtual.ts'
import { sentence } from '../domain/refusal.ts'

type Page<T> = { rows: T[]; total: number }

function sparsePages<T>(
  loadPage: (offset: number) => Promise<{ rows: T[]; total: number }>,
  keys: Ref[],
  enabled?: Ref<boolean>,
  landed?: (page: Page<T>) => void,
) {
  const loaded = shallowRef(new Map<number, T>())
  const total = ref<number | null>(null)
  const failure = ref('')
  let generation = 0
  let replacing = 0
  let fresh = new Map<number, T>()
  const asked = new Set<number>()
  const failed = new Set<number>()

  async function load(chunk: number) {
    if (enabled && !enabled.value) return
    if (asked.has(chunk)) return
    asked.add(chunk)
    const mine = generation
    try {
      const answer = await loadPage(chunk * CHUNK)
      if (mine !== generation) return
      answer.rows.forEach((row, at) => fresh.set(chunk * CHUNK + at, row))
      const swap = replacing === mine && chunk === 0
      if (swap) replacing = 0
      // A visible later chunk can beat page zero after a result-set change.
      // Page zero replaces old rows, but must retain every response already
      // landed for this generation or that chunk stays permanently missing:
      // it is still in `asked`, so the virtual grid correctly will not ask
      // for it twice.
      const next = swap ? new Map(fresh) : new Map(loaded.value)
      answer.rows.forEach((row, at) => next.set(chunk * CHUNK + at, row))
      loaded.value = next
      total.value = answer.total
      landed?.(answer)
      failed.delete(chunk)
      if (failed.size === 0) failure.value = ''
    } catch (cause) {
      if (mine !== generation) return
      asked.delete(chunk)
      failed.add(chunk)
      failure.value = sentence(cause)
    }
  }

  watch(
    [...keys, () => enabled?.value ?? true],
    () => {
      generation += 1
      replacing = generation
      fresh = new Map()
      asked.clear()
      failed.clear()
      failure.value = ''
      if (enabled && !enabled.value) {
        loaded.value = new Map()
        total.value = null
        return
      }
      void load(0)
    },
    { immediate: true },
  )
  onScopeDispose(() => (generation += 1))
  return {
    loaded,
    total,
    failure,
    need: (chunks: number[]) => chunks.forEach((chunk) => void load(chunk)),
    retry: () => {
      failure.value = ''
      for (const chunk of failed) void load(chunk)
    },
  }
}

export function useArtists(
  library: Ref<string>,
  query: Ref<string>,
  sort: Ref<string>,
  enabled?: Ref<boolean>,
) {
  const pages = sparsePages<ArtistSummary>(
    async (offset) => {
      const answer = await listArtists({
        library: library.value,
        ...(query.value ? { q: query.value } : {}),
        sort: sort.value,
        limit: CHUNK,
        offset,
      })
      return { rows: answer.artists, total: answer.total }
    },
    [library, query, sort],
    enabled,
  )
  // Preserve rows while sorting/filtering one library, but never carry
  // clickable artists across a library route change.
  watch(
    library,
    () => {
      pages.loaded.value = new Map()
      pages.total.value = null
    },
    { immediate: true },
  )
  return pages
}

export function useArtistAlbums(
  library: Ref<string>,
  key: Ref<string>,
  query: Ref<string>,
  sort: Ref<string>,
) {
  const artist = ref<ArtistSummary | null>(null)
  const pages = sparsePages<ItemRowI64>(
    async (offset) => {
      const answer = await artistAlbums(key.value, {
        library: library.value,
        ...(query.value ? { q: query.value } : {}),
        sort: sort.value,
        limit: CHUNK,
        offset,
      })
      return { rows: answer.albums, total: answer.total, artist: answer.artist }
    },
    [library, key, query, sort],
    undefined,
    (answer) => {
      artist.value = (answer as Page<ItemRowI64> & { artist: ArtistSummary }).artist
    },
  )
  // A sort or filter may leave the still-relevant old page visible until its
  // replacement arrives. A different artist may not: those cards would be
  // clickable under the new route and open with the wrong artist context.
  watch(
    [library, key],
    () => {
      artist.value = null
      pages.loaded.value = new Map()
      pages.total.value = null
    },
    { immediate: true },
  )
  return { artist, ...pages }
}
