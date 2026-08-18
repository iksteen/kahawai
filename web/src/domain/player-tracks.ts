import type { PlaybackStreams } from '../api/generated/model/playbackStreams.ts'
import type { TrackListing } from '../api/generated/model/trackListing.ts'

/// The shapes the pickers show. Structural rather than imported: the hub sends
/// these inside a source's streams, and this only ever lists them.
export type AudioChoice = { codec: string; channels: number; language?: string | null }
export type VideoChoice = {
  codec: string
  width: number
  height: number
  display_width?: number | null
  display_height?: number | null
  orientation?: string | null
}

/// What is being played and with which tracks.
///
/// Nine slots in the component, all describing one thing: the lists arrive from
/// one fetch, a switch moves a selection and the verdict and the epoch
/// together, and a restart moves the epoch alone. As nine they could disagree —
/// the audio picker showing a track the verdict says is not being served.
export type Tracks = {
  subs: TrackListing[]
  /// The chosen subtitle's id as a string, `''` for none. A string because it
  /// is a `<select>` value, and the empty option has to mean something.
  subKey: string
  audioList: AudioChoice[]
  audio: number
  videoList: VideoChoice[]
  video: number
  /// Bumped whenever the run's origin moves, so the <track> URL reloads with
  /// the new cue shift and the renderers rebuild.
  epoch: number
  /// What the hub says it is actually serving, which a track switch re-plans.
  streams: PlaybackStreams | null
  /// The live tap yielded nothing; use the flattened .vtt instead. Cleared
  /// whenever the chosen track changes.
  vttFallback: boolean
}

/// The live track choice at the moment a restart replaced the session, so the
/// remounted picture can put the viewer back where they WERE — not where the
/// preferences they started with would put them. The prefs the page holds are
/// a snapshot from session start; a subtitle picked mid-episode lives only
/// here and on the server, and re-resolving from the snapshot after a
/// recovery/stand-by/caps restart silently reverted it.
export type CarriedTracks = {
  audio: number
  video: number
  /// '' carries "none showing", which is as much a state as any track: a
  /// viewer who turned subtitles off mid-film must not get them back because
  /// the wishlist would have picked some.
  subKey: string
}

export type TrackEvent =
  | { type: 'lists-arrived'; audioList: AudioChoice[]; videoList: VideoChoice[] }
  | { type: 'audio-known'; audio: number }
  | { type: 'subtitles-arrived'; subs: TrackListing[] }
  /// From the picker, or from the opening choice — which must not override a
  /// selection the viewer already made.
  | { type: 'subtitle-chosen'; key: string; onlyIfUnset?: boolean }
  | { type: 'tracks-chosen'; audio: number; video: number }
  | { type: 'streams-known'; streams: PlaybackStreams }
  /// The run's origin moved: reload cues, rebuild renderers.
  | { type: 'run-moved' }
  | { type: 'tap-empty' }

export function initialTracks(streams: PlaybackStreams | null): Tracks {
  return {
    subs: [],
    subKey: '',
    audioList: [],
    audio: 0,
    videoList: [],
    video: 0,
    epoch: 0,
    streams,
    vttFallback: false,
  }
}

export function tracks(s: Tracks, e: TrackEvent): Tracks {
  switch (e.type) {
    case 'lists-arrived':
      return { ...s, audioList: e.audioList, videoList: e.videoList }
    case 'audio-known':
      return { ...s, audio: e.audio }
    case 'subtitles-arrived':
      return { ...s, subs: e.subs }
    case 'subtitle-chosen':
      if (e.onlyIfUnset && s.subKey) return s
      // A different track has its own tap; whatever the last one concluded
      // about the tap does not carry over.
      return { ...s, subKey: e.key, vttFallback: false }
    case 'tracks-chosen':
      return { ...s, audio: e.audio, video: e.video }
    case 'streams-known':
      return { ...s, streams: e.streams }
    case 'run-moved':
      return { ...s, epoch: s.epoch + 1 }
    case 'tap-empty':
      return { ...s, vttFallback: true }
  }
}
