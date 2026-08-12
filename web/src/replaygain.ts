/// ReplayGain (HUB-19): turn a track's stated loudness into a gain
/// factor for playback.
///
/// Album gain when playing an album, track gain otherwise. The
/// difference is the point of storing both: per-track levelling across
/// a record flattens the quiet tracks the artist meant to be quiet,
/// while a shuffled queue wants every track at the same loudness.
import type { Item } from './api'

export type GainMode = 'album' | 'track'

/// One thing in the queue: what to play and how to level it.
///
/// The mode travels with the entry rather than with the queue, because a
/// queue can hold both. A record added whole wants album gain, so the
/// quiet track the artist meant to be quiet stays quiet; a single track
/// dropped in beside it wants track gain, so it does not arrive at a
/// different loudness from its neighbours.
export type QueueEntry = { track: Item; gain: GainMode }

/// The linear factor to multiply the signal by, or 1 when the file says
/// nothing. Never louder than the peak allows: a positive gain applied
/// to a track that already reaches full scale would clip it, so the
/// gain is capped at whatever leaves the peak at 1.0.
export function replayGainFactor(item: Item | undefined, mode: GainMode): number {
  const rg = item?.replay_gain
  if (!rg) return 1
  // Fall back to the other measurement rather than to nothing: an album
  // rip missing its album tags still levels better by track than not at
  // all.
  const db =
    mode === 'album'
      ? (rg.album_gain_db ?? rg.track_gain_db)
      : (rg.track_gain_db ?? rg.album_gain_db)
  if (db == null || !Number.isFinite(db)) return 1
  const peak =
    mode === 'album' ? (rg.album_peak ?? rg.track_peak) : (rg.track_peak ?? rg.album_peak)
  const factor = 10 ** (db / 20)
  if (peak && peak > 0 && factor * peak > 1) return 1 / peak
  return factor
}
