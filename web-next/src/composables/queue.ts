/// The play queue, at module scope.
///
/// It outlives every screen: putting a record on and then browsing for the
/// next one is the ordinary way to use this, so the queue cannot belong to the
/// page that started it. Module scope rather than a store because it is one
/// value with five operations — the plan's rule is that Pinia arrives when
/// that stops being true.
///
/// The SESSIONS this queue plays through belong to the player component; this
/// holds only what is queued and where it is.

import { computed, readonly, ref } from 'vue'

import {
  advance,
  appendAlbum as append,
  appendTrack as appendOne,
  current,
  EMPTY,
  playAlbum as play,
  type Queue,
  removeAt,
  upNext,
} from '../domain/queue.ts'
import type { ItemRowI64 } from '../api/generated/model/itemRowI64.ts'
import { notify } from './notices.ts'

const queue = ref<Queue>(EMPTY)

/// Which queue this IS. Bumped when a different record is put on, and never by
/// appending: the boundary around the player is keyed on it, so a caught throw
/// clears when the record changes — and appending to the one playing must not
/// remount it mid-track.
///
/// A generation rather than the first track's id: putting the same record on
/// again produces the same first track, so a caught throw stayed caught and
/// pressing Play did nothing at all, silently, while the item page still marked
/// a track as playing.
const generation = ref(0)

export function useQueue() {
  return {
    queue: readonly(queue),
    generation: readonly(generation),
    playing: computed(() => current(queue.value)),
    next: computed(() => upNext(queue.value)),

    /// Playing a record replaces the queue; adding one leaves what is playing
    /// alone. Both are what somebody asked for.
    playAlbum(tracks: ItemRowI64[], from = 0) {
      generation.value += 1
      queue.value = play(tracks, from)
    },
    appendAlbum(tracks: ItemRowI64[]) {
      queue.value = append(queue.value, tracks)
      notify(`Added ${tracks.length} ${tracks.length === 1 ? 'track' : 'tracks'} to the queue.`)
    },
    appendTrack(track: ItemRowI64) {
      queue.value = appendOne(queue.value, track)
      notify(`Added ${track.title} to the queue.`)
    },
    /// UI-2.
    remove(index: number) {
      queue.value = removeAt(queue.value, index)
    },
    jump(index: number) {
      if (index >= 0 && index < queue.value.entries.length)
        queue.value = { ...queue.value, at: index }
    },
    /// True when it moved. The caller is the player, and "the record has
    /// finished" is a different thing from "the next track is starting".
    step(by: 1 | -1): boolean {
      const moved = advance(queue.value, by)
      if (moved) queue.value = moved
      return moved !== null
    },
    clear: clearQueue,
  }
}

/// Put it down. Both the ✕ on the dock and the end of a session, and the
/// generation moves for both: the boundary around the dock is keyed on it, and
/// putting the queue down is exactly when a caught throw should clear.
///
/// The next account must not inherit tracks it cannot read either: the queue
/// holds item ids, and the dock would ask for them and be refused for ever.
export function clearQueue() {
  generation.value += 1
  queue.value = EMPTY
}
