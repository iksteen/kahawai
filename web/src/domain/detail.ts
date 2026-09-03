/// What an item page shows about itself, and how a series is laid out.
///
/// The presentation of a series is the interesting part: HUB-31 gives
/// absolute-numbered episodes a TVDB-style projection onto seasons, and which
/// numbering is on screen has to be the same on the show page and the season
/// page — a viewer who opens "Season 2" must not land on a different set of
/// episodes than the list they clicked it from.

import { episodeOf, seasonOf } from './label.ts'

type Episode = {
  season: number | null
  proj_season: number | null
  episode: number | null
  proj_episode: number | null
  played: boolean
}

export type Disc<T> = {
  number: number
  entries: { track: T; albumIndex: number }[]
}

/// Split an album's already-ordered child rows into physical discs. A missing
/// DISCNUMBER is the ordinary single-disc shape, so it belongs to disc 1; this
/// also keeps a partly tagged release's unnumbered first disc together with
/// tracks explicitly stamped `1`.
export function discsIn<T extends { season: number | null }>(tracks: T[]): Disc<T>[] {
  const groups = new Map<number, Disc<T>>()
  tracks.forEach((track, albumIndex) => {
    const number = track.season ?? 1
    const disc = groups.get(number) ?? { number, entries: [] }
    disc.entries.push({ track, albumIndex })
    groups.set(number, disc)
  })
  return [...groups.values()]
}

/// Artwork follows what the artwork IS, not what the page is about: a track's
/// is its album's square sleeve, an episode's is a 16:9 still, and everything
/// else has a poster.
export function artShape(kind: string): { width: string; ratio: string } {
  if (kind === 'album' || kind === 'track') return { width: '180px', ratio: '1' }
  if (kind === 'episode') return { width: '320px', ratio: '16 / 9' }
  return { width: '190px', ratio: '2 / 3' }
}

/// In the order the projection puts them, when there is one. Without a
/// projection the hub's order is already right and re-sorting would be a
/// second opinion.
export function ordered<T extends Episode>(episodes: T[], projected: boolean): T[] {
  if (!projected) return episodes
  return [...episodes].sort(
    (a, b) =>
      (seasonOf(a, true) ?? 999) - (seasonOf(b, true) ?? 999) ||
      (episodeOf(a, true) ?? 0) - (episodeOf(b, true) ?? 0),
  )
}

/// The seasons a list of episodes falls into, in the order they appear.
export function seasonsIn<T extends Episode>(episodes: T[], projected: boolean): (number | null)[] {
  return [...new Set(episodes.map((e) => seasonOf(e, projected)))]
}

/// Where to carry on: the first episode nobody has watched.
///
/// `undefined` while the list has not answered. "Start from the beginning" is
/// the wrong answer to "we have not asked yet", and it flashed in as the list
/// arrived.
export function continueAt<T extends Episode>(episodes: T[] | null): T | undefined {
  return episodes?.find((e) => !e.played)
}

/// "12 episodes · 4 watched", and nothing at all until the list has answered.
///
/// A count of zero is a FACT: printing it before asking made every album read
/// "0 tracks" with both actions disabled for a round trip — disabled because
/// the data was absent, which is the one thing a disabled control must not
/// mean.
export function childCount(
  children: { played: boolean }[] | null,
  one: string,
  many: string,
): string {
  if (children === null) return ''
  const watched = children.filter((c) => c.played).length
  return `${children.length} ${children.length === 1 ? one : many} · ${watched} watched`
}
