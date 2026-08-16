/// How an item names itself in a list, and how far through it you are.
///
/// Shared because the same episode appears on the home screen, in a library
/// grid and on its show's page, and a viewer who sees "E11" in one place and
/// "S?E11" in another has to work out whether they are the same thing.
///
/// The season-projection helpers arrived with the item pages that need them
/// (phase 9). The player's own timing helpers are still to come.

import type { ItemRowI64 } from '../api/generated/model/itemRowI64.ts'

/// What every list needs from a row, and no more. Typed structurally rather
/// than as the whole generated row, so a card component can be tested with the
/// four fields it reads instead of a thirty-field fixture.
export type Labelled = Pick<
  ItemRowI64,
  | 'kind'
  | 'title'
  | 'artist'
  | 'year'
  | 'season'
  | 'episode'
  | 'episode_end'
  | 'parent_id'
  | 'parent_title'
  | 'id'
  | 'resume_position_ms'
  | 'resume_duration_ms'
>

/// S01E02 for seasoned episodes; E11 for absolute numbering (anime); E01-02
/// for a batch file spanning a range.
///
/// A null season means absolute numbering, not a missing season — so it prints
/// E11, never S?E11. Anime is numbered that way on purpose, and the question
/// mark reads as data we failed to load.
export function seLabel(season: number | null, episode: number | null, end?: number | null) {
  let e = `E${String(episode ?? 0).padStart(2, '0')}`
  if (end != null) e += `-${String(end).padStart(2, '0')}`
  return season === null ? e : `S${String(season).padStart(2, '0')}${e}`
}

/// The mono line under a card: whatever distinguishes this item from the
/// others beside it. An episode needs to say which show it is from — a row
/// called "Pilot" is otherwise one of eight.
export function metaLine(i: Labelled): string {
  if (i.kind === 'episode') {
    return [i.parent_title, seLabel(i.season, i.episode, i.episode_end)].filter(Boolean).join(' ')
  }
  if (i.kind === 'track') return [i.artist, i.parent_title].filter(Boolean).join(' · ')
  // An album is told apart by who made it AND when — there are four Greatest
  // Hits by four bands and three pressings of one of them.
  if (i.kind === 'album') return [i.artist, i.year].filter(Boolean).join(' · ')
  return i.year ? String(i.year) : ''
}

/// Where an item's own page lives. Tracks have no page of their own, so a
/// track opens its album.
export function targetOf(i: Pick<Labelled, 'kind' | 'id' | 'parent_id'>): string {
  return i.kind === 'track' && i.parent_id ? i.parent_id : i.id
}

/// How far through, as a percentage, or null when it has not been started or
/// the duration is unknown.
export function watchedPct(
  i: Pick<Labelled, 'resume_position_ms' | 'resume_duration_ms'>,
): number | null {
  if (!i.resume_position_ms || !i.resume_duration_ms) return null
  return Math.min(100, (i.resume_position_ms / i.resume_duration_ms) * 100)
}

/// HUB-31: absolute-numbered (anime) episodes carry a TVDB-style projection
/// onto seasons. Whether to use it is a user preference, but the answer has to
/// be the same on the show page and the season page — a viewer who opens
/// "Season 2" must not land on a different set of episodes than the list they
/// clicked it from.
export function projecting(
  animeView: 'seasons' | 'native',
  episodes: { proj_season: number | null }[],
): boolean {
  return animeView === 'seasons' && episodes.some((e) => e.proj_season != null)
}

export function seasonOf(
  episode: { season: number | null; proj_season: number | null },
  projected: boolean,
): number | null {
  return projected ? (episode.proj_season ?? null) : episode.season
}

export function episodeOf(
  episode: { episode: number | null; proj_episode: number | null },
  projected: boolean,
): number | null {
  return projected ? (episode.proj_episode ?? episode.episode) : episode.episode
}

/// A null season means absolute numbering, which is a different thing from
/// Specials (season 0) and from a season nobody named.
export function seasonLabel(season: number | null, projected: boolean): string {
  if (season === null) return projected ? 'Other' : 'Episodes'
  return season === 0 ? 'Specials' : `Season ${season}`
}

/// Where pressing Play should start: the stored position, or 0 once it is
/// close enough to the end that resuming would mean watching the credits.
///
/// Off `resume_duration_ms`, which is the hub's own figure for the WHOLE item.
/// Computing it from the largest single file instead silently loses the resume
/// on every multi-part film: forty minutes into a seven-part feature is past
/// 90% of part one, so Play restarted from zero.
export function resumeMs(item: {
  resume_position_ms: number | null
  resume_duration_ms: number | null
}): number {
  const at = item.resume_position_ms
  const total = item.resume_duration_ms
  if (!at || !total) return 0
  return at < total * 0.9 ? at : 0
}
