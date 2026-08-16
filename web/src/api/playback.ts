/// Starting, steering and addressing a playback session.
///
/// The generated bindings take wire shapes; these take what the player has —
/// an item, a position, a track — and know the two things the wire cannot: what
/// this browser can be served (the capability profile) and where a session's
/// sibling files live.

import type { CapabilityProfile } from './generated/model/capabilityProfile.ts'
import type { ItemQueryResponse } from './generated/model/itemQueryResponse.ts'
import type { PlaybackStreams } from './generated/model/playbackStreams.ts'
import type { Preference } from './generated/model/preference.ts'
import type { StartSessionResponse } from './generated/model/startSessionResponse.ts'
import type { TrackListing } from './generated/model/trackListing.ts'
import {
  getItemFontUrl,
  getPrefs,
  getItemSubtitleFileUrl,
  getSessionFileUrl,
  seekSession as seek,
  startSession,
} from './generated/kahawai.ts'
import { buildProfile } from './capabilities.ts'
import { isRasterSub } from '../domain/subtitles.ts'

/// Start a session for an item, with everything the hub needs to negotiate.
///
/// `prefs` is REQUIRED, and `'read'` is how a caller says it has none in hand.
/// This is the only reader of `bandwidth_kbps` in the app, so a default of `[]`
/// is the cap silently dropped with nothing said anywhere — and four of the
/// five callers had no preferences to pass: every recovery, every hand-pressed
/// Try again, the capability restart and the stand-by resume. A viewer who set
/// a cap lost it the first time the session was reaped, on the metered link the
/// setting exists for.
export async function startPlaybackSession(
  item: ItemQueryResponse,
  startMs = 0,
  audioTrack = 0,
  videoTrack = 0,
  prefs: Preference[] | 'read' = 'read',
): Promise<StartSessionResponse> {
  // Swallowed, and only here: these callers are automatic — a recovery, a
  // stand-by tick every five seconds — and the ones that are a deliberate press
  // have preferences in hand already. An uncapped start is better than none.
  const known =
    prefs === 'read'
      ? await getPrefs().then(
          (r) => r.prefs,
          () => [],
        )
      : prefs
  const cap = known.find((p) => p.scope === '' && p.key === 'bandwidth_kbps')?.value
  // Source-aware precision: probe the exact strings the announced streams call
  // for, with the profile and level from the hub's own probing.
  const announced = item.sources.flatMap((source) => source.streams?.video ?? [])
  const profile: CapabilityProfile = buildProfile(cap ? Number(cap) : undefined, announced)
  return startSession({
    item_id: item.id,
    profile,
    start_ms: Math.round(startMs),
    audio_track: audioTrack,
    video_track: videoTrack,
  })
}

/// Move the pipeline, and optionally what it is muxing.
///
/// A track switch and a burn transition are both seeks: the pipeline restarts
/// at the current position with the new choice, which is the same ~2 s hiccup
/// as a deep seek.
export function seekSession(
  sessionId: string,
  positionMs: number,
  audioTrack?: number,
  videoTrack?: number,
  /// An image track id switches the burn mid-session; 0 withdraws an explicit
  /// burn; absent leaves it as it is.
  subtitleTrack?: number,
): Promise<{ part_base_ms: number; streams?: PlaybackStreams | null }> {
  return seek(sessionId, {
    position_ms: Math.round(positionMs),
    audio_track: audioTrack ?? null,
    video_track: videoTrack ?? null,
    subtitle_track: subtitleTrack ?? null,
  }) as Promise<{ part_base_ms: number; streams?: PlaybackStreams | null }>
}

/// A subtitle file on the ITEM: whole-file extraction, streamed. `shiftMs`
/// moves the cues to meet a timeline that starts mid-file.
export const subtitleFileUrl = (itemId: string, file: string, shiftMs?: number) =>
  getItemSubtitleFileUrl(itemId, file, shiftMs === undefined ? undefined : { shift_ms: shiftMs })

export const fontUrl = (itemId: string, index: number) => getItemFontUrl(itemId, index)

/// Where a track's display sets come from. An embedded image track is decoded
/// by the RUNNING pipeline and tail-followed off the session; a rasterised one
/// (HUB-32d) is a finished artefact on the item.
export const overlayUrl = (track: TrackListing, itemId: string, streamUrl: string) =>
  isRasterSub(track)
    ? getItemSubtitleFileUrl(itemId, `${track.id}.jsonl`)
    : getSessionFileUrl(streamUrl.split('/').at(-2) ?? '', `subs-${track.id}.jsonl`)

/// A file beside the playlist of the session currently running — the live
/// subtitle taps, and the pipeline's own report of where its run begins.
export const sessionFileUrl = (streamUrl: string, file: string) =>
  streamUrl.replace(/[^/]*$/, '') + file
