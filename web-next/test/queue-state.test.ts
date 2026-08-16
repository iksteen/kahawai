/// The queue at module scope: what survives navigation, and what the boundary
/// around the dock is keyed on.

import { afterEach, beforeEach, describe, expect, test } from 'vitest'

import type { ItemRowI64 } from '../src/api/generated/model/itemRowI64.ts'
import { clearNotices, notice } from '../src/composables/notices.ts'
import { clearQueue, useQueue } from '../src/composables/queue.ts'

const track = (id: string) => ({ id, title: id.toUpperCase() }) as ItemRowI64

const queue = useQueue()

beforeEach(() => {
  clearQueue()
  clearNotices()
})
afterEach(clearQueue)

describe('which queue this is', () => {
  test('a different record is a different one', () => {
    // The dock's error boundary is keyed on this, so a caught throw clears when
    // somebody puts something else on. A generation rather than the first
    // track's id: putting the SAME record on again produces the same first
    // track, so a caught throw stayed caught and pressing Play did nothing at
    // all, silently, while the item page still marked a track as playing.
    const first = queue.generation.value
    queue.playAlbum([track('a')])
    expect(queue.generation.value).toBeGreaterThan(first)
    const second = queue.generation.value
    queue.playAlbum([track('a')])
    expect(queue.generation.value).toBeGreaterThan(second)
  })

  test('but appending to the one playing is NOT', () => {
    // Remounting the dock mid-track is the thing the generation exists to
    // prevent.
    queue.playAlbum([track('a')])
    const gen = queue.generation.value
    queue.appendAlbum([track('b')])
    queue.appendTrack(track('c'))
    queue.jump(1)
    queue.remove(2)
    expect(queue.generation.value).toBe(gen)
  })

  test('and putting it down is', () => {
    queue.playAlbum([track('a')])
    const gen = queue.generation.value
    queue.clear()
    expect(queue.generation.value).toBeGreaterThan(gen)
  })
})

describe('moving about in it', () => {
  test('a jump outside it is refused rather than clamped', () => {
    // Clamping would silently start a different track from the one that was
    // pressed.
    queue.playAlbum([track('a'), track('b')])
    queue.jump(5)
    expect(queue.playing.value?.track.id).toBe('a')
    queue.jump(-1)
    expect(queue.playing.value?.track.id).toBe('a')
    queue.jump(1)
    expect(queue.playing.value?.track.id).toBe('b')
  })

  test('and stepping says whether it moved', () => {
    // The player asks, because "the record has finished" is a different thing
    // from "the next track is starting".
    queue.playAlbum([track('a'), track('b')])
    expect(queue.step(1)).toBe(true)
    expect(queue.step(1)).toBe(false)
    expect(queue.playing.value?.track.id).toBe('b')
  })
})

describe('adding to it', () => {
  test('says what was added, and how much', () => {
    queue.appendAlbum([track('a'), track('b')])
    expect(notice.value).toContain('2 tracks')
    queue.appendAlbum([track('c')])
    expect(notice.value).toContain('1 track')
    expect(notice.value).not.toContain('1 tracks')
  })

  test('and one track names itself', () => {
    queue.appendTrack(track('a'))
    expect(notice.value).toContain('A')
  })

  test('but putting a record on does not — you are looking at it', () => {
    queue.playAlbum([track('a')])
    expect(notice.value).toBe('')
  })

  test('and the next one up is the one after the one playing', () => {
    // Gapless is why this exists at all: it is the track to warm.
    queue.playAlbum([track('a'), track('b'), track('c')], 1)
    expect(queue.next.value?.track.id).toBe('c')
    queue.jump(2)
    expect(queue.next.value).toBeUndefined()
  })
})

describe('when the session ends', () => {
  test('the queue goes with it, whichever way it ended', () => {
    // An expiry is not a change of person, but nothing should still be playing
    // to a sign-in screen — and the next account in this tab would inherit a
    // queue whose tracks it may not read, which the dock retries for ever
    // because a track it may not see looks exactly like a host that is down.
    queue.playAlbum([track('a')])
    clearQueue()
    expect(queue.queue.value.entries).toHaveLength(0)
  })
})
