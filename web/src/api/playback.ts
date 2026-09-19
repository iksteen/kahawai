/// Starting, steering and addressing a playback session.
///
/// The generated bindings take wire shapes; these take what the player has —
/// an item, a position, a track — and know the two things the wire cannot: what
/// this browser can be served (the capability profile) and where a session's
/// sibling files live.

import { catalogueDetail } from './catalogue.ts'
import type { ItemQuery } from './generated/model/itemQuery.ts'
import type { CapabilityProfile } from './generated/model/capabilityProfile.ts'
import type { ItemDetail } from './catalogue-model.ts'
import type { PlaybackStreams } from './generated/model/playbackStreams.ts'
import type { Preference } from './generated/model/preference.ts'
import type { StartSessionResponse } from './generated/model/startSessionResponse.ts'
import type { TrackListing } from './generated/model/trackListing.ts'
import {
  getPrefs,
  getSessionFileUrl,
  seekSession as seek,
  startSession,
} from './generated/kahawai.ts'
import { buildProfile } from './capabilities.ts'
import { isRasterSub } from '../domain/subtitles.ts'
import { resolveTracks } from '../domain/tracks.ts'
import { sourcePreferenceScope, sourceStreams } from '../domain/source.ts'

export function playbackItem(
  id: string,
  query: ItemQuery,
  library?: string | null,
): Promise<ItemDetail> {
  if (!library) throw new Error('Playback requires a library.')
  return catalogueDetail(library, id, query)
}

function playbackProfile(item: ItemDetail, prefs: Preference[]): CapabilityProfile {
  const cap = prefs.find((p) => p.scope === '' && p.key === 'bandwidth_kbps')?.value
  const announced = item.sources.flatMap((source) => source.streams?.video ?? [])
  return buildProfile(cap ? Number(cap) : undefined, announced)
}

function sourceAudioTracks(item: ItemDetail, prefs: Preference[], mediaType: string) {
  return Object.fromEntries(
    [...new Set(item.sources.map((source) => source.source_id))].map((id) => [
      id,
      resolveTracks(
        prefs,
        item.parent_id ?? item.id,
        mediaType,
        item.metadata?.original_language,
        sourceStreams(item.sources, id)?.audio ?? [],
        sourcePreferenceScope(item.sources, id),
      ).audioTrack,
    ]),
  )
}

/// The audio-zero preview is only final when every chosen track and the
/// source-aware profile agree with it. Let the hub rank each rendition on its
/// own preferred audio index, then pin that source/index pair for START.
export async function selectPlaybackSource(
  preview: ItemDetail,
  prefs: Preference[],
  mediaType: string,
  previewProfile: CapabilityProfile,
  sourceId?: number,
) {
  let item = preview
  let profile = playbackProfile(item, prefs)
  let audio = sourceAudioTracks(item, prefs, mediaType)
  const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b)
  let needsQuery =
    !same(profile, previewProfile) ||
    (sourceId === undefined
      ? Object.values(audio).some((track) => track !== 0)
      : (audio[sourceId] ?? 0) !== 0)
  for (let attempt = 0; ; attempt++) {
    if (!needsQuery) {
      const selected = item.negotiated?.source?.source_id ?? sourceId
      return {
        item,
        profile,
        sourceId: selected,
        audioTrack: selected === undefined ? 0 : (audio[selected] ?? 0),
      }
    }
    if (attempt === 3)
      throw new Error('The available sources changed while choosing playback. Try again.')
    item = await playbackItem(
      item.id,
      {
        profile,
        source_audio_tracks: audio,
        ...(sourceId === undefined
          ? {}
          : {
              source_id: sourceId,
              media_entry_id:
                item.sources.find((s) => s.source_id === sourceId)?.media_entry_id ?? null,
            }),
      },
      item.library_id,
    )
    const nextAudio = sourceAudioTracks(item, prefs, mediaType)
    const nextProfile = playbackProfile(item, prefs)
    // A concurrent scan may return a new rendition or changed stream order.
    // Re-rank its real preference before starting, never borrow an old index.
    needsQuery = !same(nextAudio, audio) || !same(nextProfile, profile)
    audio = nextAudio
    profile = nextProfile
  }
}

/// Detail previews and subtitle defaults need the same preference-aware
/// source choice as Play. Capture the preview profile with its request so a
/// later capability change cannot make an old negotiation look current.
export async function queryPlaybackItem(
  id: string,
  prefs: Preference[],
  mediaType: string,
  sourceId?: number,
  library?: string,
  mediaEntryId?: string | null,
): Promise<ItemDetail> {
  const cap = prefs.find((pref) => pref.scope === '' && pref.key === 'bandwidth_kbps')?.value
  const profile = buildProfile(cap ? Number(cap) : undefined)
  const preview = await playbackItem(
    id,
    {
      profile,
      ...(sourceId === undefined ? {} : { source_id: sourceId }),
      media_entry_id: mediaEntryId ?? null,
    },
    library,
  )
  return (await selectPlaybackSource(preview, prefs, mediaType, profile, sourceId)).item
}

/// Start a session for an item, with everything the hub needs to negotiate.
///
/// `prefs: 'read'` fetches preferences when a caller has none in hand.
/// Recovery, Try again, capability restart and stand-by resume may have no
/// preferences to pass. Reading them here preserves the viewer's bandwidth
/// cap whenever a session is recreated.
export async function startPlaybackSession(
  item: ItemDetail,
  {
    startMs = 0,
    audioTrack = 0,
    videoTrack = 0,
    prefs = 'read',
    sourceFingerprint,
    resume = true,
    sourceId,
    profile: selectedProfile,
  }: {
    startMs?: number
    audioTrack?: number
    videoTrack?: number
    prefs?: Preference[] | 'read'
    sourceFingerprint?: string
    resume?: boolean
    sourceId?: number | undefined
    profile?: CapabilityProfile
  } = {},
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
  // Source-aware precision: probe the exact strings the announced streams call
  // for, with the profile and level from the hub's own probing.
  const profile = selectedProfile ?? playbackProfile(item, known)
  const session = await startSession({
    item_id: item.id,
    library_id: item.library_id,
    media_entry_id:
      item.sources.find((s) => s.source_id === (sourceId ?? item.negotiated?.source?.source_id))
        ?.media_entry_id ?? null,
    source_id: sourceId ?? null,
    profile,
    start_ms: Math.round(startMs),
    resume_source_fingerprint: sourceFingerprint ?? null,
    resume,
    audio_track: audioTrack,
    video_track: videoTrack,
  })
  const source = session.media_entry_id
    ? item.sources.find((s) => s.media_entry_id === session.media_entry_id)
    : undefined
  if (source) session.source_id = source.source_id
  return session
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

/// Source-bound subtitle files are addressed through the captured session.
export const subtitleFileUrl = (
  _itemId: string,
  file: string,
  shiftMs?: number,
  session?: StartSessionResponse,
) => {
  if (!session) throw new Error('Subtitle streaming requires a session.')
  return `/api/v1/playback/sessions/${session.session_id}/subtitles/${file}${shiftMs === undefined ? '' : `?shift_ms=${shiftMs}`}`
}

/// Where a track's display sets come from. An embedded image track is decoded
/// by the RUNNING pipeline and tail-followed off the session; a rasterised one
/// (HUB-32d) is a finished source artifact captured by the session.
export const overlayUrl = (
  track: TrackListing,
  itemId: string,
  streamUrl: string,
  session?: StartSessionResponse,
) =>
  isRasterSub(track)
    ? subtitleFileUrl(itemId, `${track.id}.jsonl`, undefined, session)
    : getSessionFileUrl(streamUrl.split('/').at(-2) ?? '', `subs-${track.id}.jsonl`)

/// A file beside the playlist of the session currently running — the live
/// subtitle taps, and the pipeline's own report of where its run begins.
export const sessionFileUrl = (streamUrl: string, file: string) =>
  streamUrl.replace(/[^/]*$/, '') + file
