/// HUB-33: which audio and which subtitles a session opens with.
///
/// Three layers, most specific first, because a preference is not one thing:
/// "the commentary track of THIS film" has no language representation, "this
/// series in Japanese" is portable across episodes whose mux order differs, and
/// "anime in Japanese with English subtitles" is a standing rule about a kind
/// of media. Each layer exists because the one above it cannot say what it
/// says.

import type { Preference } from '../api/generated/model/preference.ts'

export type AudioStream = { language?: string | null }

export type Resolved = {
  audioTrack: number
  /// The ordered language wishlist; `[]` leaves subtitles off.
  subs: string[]
  /// This item's exact remembered track, honoured only if it is still in the
  /// list the hub returned.
  subTrack: number | null
}

export function resolveTracks(
  prefs: Preference[],
  seriesId: string,
  itemId: string,
  mediaType: string,
  originalLanguage: string | null | undefined,
  audio: AudioStream[],
): Resolved {
  const get = (scope: string, key: string) =>
    prefs.find((p) => p.scope === scope && p.key === key)?.value
  const list = (value?: string) =>
    (value ?? '')
      .split(',')
      .map((s) => s.trim().toLowerCase())
      .filter(Boolean)
  /// Two letters is a match: `en` and `eng` and `en-GB` are the same wish.
  const langEq = (have: string | null | undefined, want: string) => {
    if (!have) return false
    const a = have.toLowerCase()
    const b = want.toLowerCase()
    return a === b || a.slice(0, 2) === b.slice(0, 2)
  }

  // Audio, most specific first: THIS item's exact track (two English tracks —
  // feature and commentary — are common, so language cannot express the
  // choice), then the series language memory, then the ordered per-type list.
  let audioTrack: number | undefined
  const exact = get(itemId, 'audio.track')
  if (exact?.startsWith('#')) {
    const at = Number(exact.slice(1))
    if (at >= 0 && at < audio.length) audioTrack = at
  }
  const remembered = get(seriesId, 'audio')
  if (audioTrack !== undefined) {
    // The exact item preference already decided.
  } else if (remembered?.startsWith('#')) {
    const at = Number(remembered.slice(1))
    if (at >= 0 && at < audio.length) audioTrack = at
  } else if (remembered) {
    const at = audio.findIndex((a) => langEq(a.language, remembered))
    if (at >= 0) audioTrack = at
  }
  if (audioTrack === undefined) {
    // 'original' is the standing backstop: the implicit final entry of every
    // audio wishlist, and the whole list when none is set.
    const wish = list(get('', `audio.${mediaType}`))
    if (!wish.includes('original')) wish.push('original')
    for (const want of wish) {
      const language = want === 'original' ? originalLanguage : want
      if (!language) continue
      const at = audio.findIndex((a) => langEq(a.language, language))
      if (at >= 0) {
        audioTrack = at
        break
      }
    }
  }

  // Subtitles: the memory ('off' | 'any' | a language), else the per-type list.
  const remembers = get(seriesId, 'subs')
  const subs =
    remembers === 'off' ? [] : remembers ? [remembers] : list(get('', `subs.${mediaType}`))
  // Top precedence (subtitle unification): THIS item's exact track id — the
  // only spelling that can name a specific downloaded or OCR row.
  const exactSub = get(itemId, 'subs.track')
  const subTrack = exactSub && /^\d+$/.test(exactSub) ? Number(exactSub) : null
  return { audioTrack: audioTrack ?? 0, subs, subTrack }
}

/// One uniform label across origins; delivery adds the honest suffix — a burn
/// restarts the session, and `none` renders disabled.
export function subtitleLabel(track: {
  language?: string | null
  format: string
  origin: string
  delivery: string
}): string {
  const origin =
    track.origin === 'sidecar'
      ? ' · file'
      : track.origin === 'downloaded'
        ? ' · downloaded'
        : track.origin === 'ocr'
          ? ' · ocr'
          : track.origin === 'raster'
            ? ' · typeset'
            : ''
  const delivery =
    track.delivery === 'burn' ? ' · burn-in' : track.delivery === 'none' ? ' · unavailable' : ''
  return `${track.language ?? 'unknown'} · ${track.format}${origin}${delivery}`
}
