/// The play queue, as data: what is in it, what is playing, and the four
/// things that can be done to it.
///
/// HUB-19: ReplayGain turns a track's stated loudness into a gain factor.
/// Album gain when playing a record, track gain otherwise — which is the point
/// of storing both. Per-track levelling across a record flattens the quiet
/// track the artist meant to be quiet; a queue of singles wants every track at
/// the same loudness.

import type { ItemRowI64 } from '../api/generated/model/itemRowI64.ts'

export type GainMode = 'album' | 'track'

/// One thing in the queue: what to play and how to level it.
///
/// The mode travels with the ENTRY rather than with the queue, because a queue
/// can hold both. A record added whole wants album gain; a single track
/// dropped in beside it wants track gain, so it does not arrive at a different
/// loudness from its neighbours.
export type QueueEntry = { track: ItemRowI64; gain: GainMode }

export type Queue = { entries: QueueEntry[]; at: number }

export const EMPTY: Queue = { entries: [], at: 0 }

/// The linear factor to multiply the signal by, or 1 when the file says
/// nothing.
///
/// Never louder than the peak allows: a positive gain applied to a track that
/// already reaches full scale would clip it, so it is capped at whatever
/// leaves the peak at 1.0.
export function replayGainFactor(track: ItemRowI64 | undefined, mode: GainMode): number {
  const gain = track?.replay_gain
  if (!gain) return 1
  // Fall back to the other measurement rather than to nothing: an album rip
  // missing its album tags still levels better by track than not at all.
  const db =
    mode === 'album'
      ? (gain.album_gain_db ?? gain.track_gain_db)
      : (gain.track_gain_db ?? gain.album_gain_db)
  if (db == null || !Number.isFinite(db)) return 1
  const peak =
    mode === 'album' ? (gain.album_peak ?? gain.track_peak) : (gain.track_peak ?? gain.album_peak)
  const factor = 10 ** (db / 20)
  if (peak && peak > 0 && factor * peak > 1) return 1 / peak
  return factor
}

/// Playing a record replaces the queue; adding one leaves what is playing
/// alone. Both are what somebody asked for, and neither is the other.
export function playAlbum(tracks: ItemRowI64[], from: number): Queue {
  return { entries: tracks.map((track) => ({ track, gain: 'album' })), at: from }
}

export function appendAlbum(queue: Queue, tracks: ItemRowI64[]): Queue {
  return {
    entries: [...queue.entries, ...tracks.map((track): QueueEntry => ({ track, gain: 'album' }))],
    at: queue.at,
  }
}

/// One track, levelled by itself: it is not arriving as part of a record.
export function appendTrack(queue: Queue, track: ItemRowI64): Queue {
  return { entries: [...queue.entries, { track, gain: 'track' }], at: queue.at }
}

/// UI-2: drop one entry.
///
/// What happens to the position is the whole question. Removing something
/// BEFORE what is playing shifts it down by one and must not change what is
/// playing; removing what IS playing moves to whatever takes its place, which
/// is the next track — and at the end of the queue, to nothing.
export function removeAt(queue: Queue, index: number): Queue {
  if (index < 0 || index >= queue.entries.length) return queue
  const entries = queue.entries.filter((_, at) => at !== index)
  if (entries.length === 0) return EMPTY
  const at = index < queue.at ? queue.at - 1 : Math.min(queue.at, entries.length - 1)
  return { entries, at }
}

/// Where the queue goes when a track ends, or when Next is pressed. Past the
/// last track it stops rather than wrapping: a record that has finished has
/// finished.
export function advance(queue: Queue, by: 1 | -1): Queue | null {
  const at = queue.at + by
  if (at < 0 || at >= queue.entries.length) return null
  return { ...queue, at }
}

export function current(queue: Queue): QueueEntry | undefined {
  return queue.entries[queue.at]
}

/// The one after it, which is the one to warm up. Gapless playback (HUB-19) is
/// why this exists at all: preparing the next track once the current one ends
/// costs a session round trip plus however long the element needs to buffer —
/// audible on every boundary, and worst exactly where it matters, on a record
/// mixed to run continuously.
export function upNext(queue: Queue): QueueEntry | undefined {
  return queue.entries[queue.at + 1]
}
