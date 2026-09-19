/// One item, its children, and the marks you can put on them.
///
/// UI-13 is the shape of this file. Three failures live on an item page and
/// they are three different things:
///
/// - **the item did not load** — there is no page, so this one takes the
///   screen;
/// - **the children did not load** — the head is real and already on screen,
///   so this is a line where the list would be;
/// - **something you asked for did not work** — a refused Play, a mark that
///   would not stick. The page is intact and you are still looking at it, so
///   this is a line ON it, never a replacement for it.
///
/// One `error` state used to do two of those jobs, and a Play the hub refused
/// replaced the whole item page with "Could not load this item" — which was
/// false: the item had loaded, and it was the play that failed.

import { computed, type Ref, ref } from 'vue'
import { queryOptions, useInfiniteQuery, useQuery, useQueryClient } from '@tanstack/vue-query'

import { catalogueChildren } from '../api/catalogue.ts'
import type { ItemDetail } from '../api/catalogue-model.ts'
import type { Preference } from '../api/generated/model/preference.ts'
import { queryPlaybackItem } from '../api/playback.ts'
import { catalogueSetWatched } from '../api/generated/kahawai.ts'
import { notify } from './notices.ts'
import { sentence } from '../domain/refusal.ts'

/// What this client would actually be served, for the profile it asked with.
///
/// A QUERY rather than a GET: the answer depends on what the client can play,
/// and the profile is a body. The verdicts it comes back with are the hub's
/// own — the point of asking the item what it would serve is that the answer
/// comes from the code that will serve it.
export function playbackItemQuery(
  id: string,
  library: string | undefined,
  prefs: Preference[],
  mediaType: string,
  source?: number,
  mediaEntryId?: string | null,
) {
  return queryOptions({
    queryKey: ['item', id, library, { prefs, mediaType, source, mediaEntryId }],
    queryFn: (): Promise<ItemDetail> =>
      queryPlaybackItem(id, prefs, mediaType, source, library, mediaEntryId),
  })
}

export function useItem(
  id: Ref<string>,
  playback?: {
    prefs: Ref<Preference[]>
    mediaType: Ref<string>
    ready: Ref<boolean>
  },
  library?: Ref<string>,
) {
  return useQuery(
    computed(() => ({
      ...playbackItemQuery(
        id.value,
        library?.value,
        playback?.prefs.value ?? [],
        playback?.mediaType.value ?? '',
      ),
      // An unopened episode panel has no item to ask for.
      enabled: id.value !== '' && (playback?.ready.value ?? true),
    })),
  )
}

/// A show's episodes or an album's tracks.
///
/// Its own query, so a list that fails has something to retry that is not the
/// item — the item does not change when a retry is what you want, and sharing
/// one attempt meant a track list that failed once could not be asked for
/// again.
export function useChildren(
  item: Ref<{ id: string; kind: string; library_id?: string | null } | undefined>,
) {
  return useChildPages(
    computed(() => item.value?.id ?? ''),
    computed(() => item.value?.library_id),
    computed(() => item.value?.kind === 'series' || item.value?.kind === 'album'),
  )
}

export function useChildrenOf(id: Ref<string>, library?: Ref<string>, season?: Ref<number | null>) {
  return useChildPages(
    id,
    library,
    computed(() => id.value !== ''),
    season,
  )
}

function useChildPages(
  id: Ref<string>,
  library: Ref<string | null | undefined> | undefined,
  enabled: Ref<boolean>,
  season?: Ref<number | null>,
) {
  const query = useInfiniteQuery({
    queryKey: computed(() => ['children', id.value, library?.value, season?.value]),
    enabled,
    initialPageParam: 0,
    queryFn: async ({ pageParam }) => {
      if (!library?.value) throw new Error('Children require a library.')
      {
        const page = await catalogueChildren(library.value, id.value, {
          offset: pageParam,
          limit: 200,
          ...(season ? { season: season.value === null ? 'absolute' : String(season.value) } : {}),
        })
        return {
          ...page,
          offset: page.offset ?? 0,
          total: page.total ?? page.children.length,
          groups: page.groups ?? [],
        }
      }
    },
    getNextPageParam: (last) => {
      const next = last.offset + last.children.length
      return last.children.length && next < last.total ? next : undefined
    },
  })
  return {
    ...query,
    data: computed(() => query.data.value?.pages.flatMap((page) => page.children)),
    total: computed(() => query.data.value?.pages[0]?.total),
    groups: computed(() => query.data.value?.pages[0]?.groups ?? []),
  }
}

/// Ticking something off, and taking the tick back.
///
/// Reported rather than thrown: the page is intact and the control that caused
/// it is still on screen, so pressing it again IS the retry (UX-1). What it
/// must not do is leave the tick showing a state the hub does not hold, so
/// everything it touched is asked again.
export function useWatched(library?: Ref<string>) {
  const client = useQueryClient()
  const busy = ref(new Set<string>())

  async function mark(
    id: string,
    played: boolean,
    items?: string[],
    season?: number | null,
  ): Promise<boolean> {
    if (busy.value.has(id)) return false
    busy.value = new Set(busy.value).add(id)
    try {
      if (!library?.value) throw new Error('Watch state requires a library.')
      {
        await catalogueSetWatched(library.value, id, {
          played,
          ...(season !== undefined
            ? { season: season === null ? 'absolute' : String(season) }
            : items
              ? { items }
              : {}),
        })
      }
      // Both, because a mark changes the child's own row and the parent's
      // count of watched children.
      //
      // The RE-ASK may fail on its own, and that is not this write failing:
      // the mark landed. Reported as a notice, because the page is intact and
      // what it is showing is merely a moment out of date.
      const asked = await Promise.all([
        client.invalidateQueries({ queryKey: ['children'] }),
        client.invalidateQueries({ queryKey: ['item'] }),
        client.invalidateQueries({ queryKey: ['shelf'] }),
        client.invalidateQueries({ queryKey: ['search'] }),
        client.invalidateQueries({ queryKey: ['continuing'] }),
        client.invalidateQueries({ queryKey: ['up-next'] }),
      ]).then(
        () => true,
        () => false,
      )
      if (!asked) notify('Marked, but could not re-read it — this may be a moment out of date.')
      return true
    } catch (cause) {
      notify(`Could not change the watched mark: ${sentence(cause)}`)
      return false
    } finally {
      const next = new Set(busy.value)
      next.delete(id)
      busy.value = next
    }
  }

  return { mark, busy }
}
