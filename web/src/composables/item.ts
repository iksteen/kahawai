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
import { useQuery, useQueryClient } from '@tanstack/vue-query'

import { buildProfile } from '../api/capabilities.ts'
import type { ItemQueryResponse } from '../api/generated/model/itemQueryResponse.ts'
import { itemChildren, itemQuery, itemSetWatched } from '../api/generated/kahawai.ts'
import { notify } from './notices.ts'
import { sentence } from '../domain/refusal.ts'

/// What this client would actually be served, for the profile it asked with.
///
/// A QUERY rather than a GET: the answer depends on what the client can play,
/// and the profile is a body. The verdicts it comes back with are the hub's
/// own — the point of asking the item what it would serve is that the answer
/// comes from the code that will serve it.
export function useItem(id: Ref<string>) {
  return useQuery({
    queryKey: computed(() => ['item', id.value]),
    // Nothing is asked for an id nobody has chosen — the season page's open
    // panel has none until a still is picked, and asking anyway sent a QUERY
    // for the empty id on every visit and after every mark.
    enabled: computed(() => id.value !== ''),
    queryFn: (): Promise<ItemQueryResponse> => itemQuery(id.value, { profile: buildProfile() }),
  })
}

/// A show's episodes or an album's tracks.
///
/// Its own query, so a list that fails has something to retry that is not the
/// item — the item does not change when a retry is what you want, and sharing
/// one attempt meant a track list that failed once could not be asked for
/// again.
export function useChildren(item: Ref<{ id: string; kind: string } | undefined>) {
  return useQuery({
    queryKey: computed(() => ['children', item.value?.id ?? '']),
    enabled: computed(() => item.value?.kind === 'show' || item.value?.kind === 'album'),
    queryFn: () => itemChildren(item.value!.id),
    select: (answer) => answer.children,
  })
}

/// The same list, asked for by ID rather than by item.
///
/// The season page knows the show id from its own URL, so waiting for the item
/// to answer before asking would put a round trip in front of every still —
/// and on that page the episodes ARE the page, while the item supplies only
/// the title on the back button.
export function useChildrenOf(id: Ref<string>) {
  return useQuery({
    queryKey: computed(() => ['children', id.value]),
    enabled: computed(() => id.value !== ''),
    queryFn: () => itemChildren(id.value),
    select: (answer) => answer.children,
  })
}

/// Ticking something off, and taking the tick back.
///
/// Reported rather than thrown: the page is intact and the control that caused
/// it is still on screen, so pressing it again IS the retry (UX-1). What it
/// must not do is leave the tick showing a state the hub does not hold, so
/// everything it touched is asked again.
export function useWatched() {
  const client = useQueryClient()
  const busy = ref(new Set<string>())

  async function mark(id: string, played: boolean, items?: string[]): Promise<boolean> {
    if (busy.value.has(id)) return false
    busy.value = new Set(busy.value).add(id)
    try {
      await itemSetWatched(id, items ? { played, items } : { played })
      // Both, because a mark changes the child's own row and the parent's
      // count of watched children.
      //
      // The RE-ASK may fail on its own, and that is not this write failing:
      // the mark landed. Reported as a notice, because the page is intact and
      // what it is showing is merely a moment out of date.
      const asked = await Promise.all([
        client.invalidateQueries({ queryKey: ['children'] }),
        client.invalidateQueries({ queryKey: ['item'] }),
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
