/// What the Background work section says about a queue, with no framework in it.
///
/// The hub reports every queue in one shape (`/admin/v1/work`): counts per
/// state, the next time the clock can make a row claimable, the last error,
/// and whether an administrator can rerun it. This module groups them by
/// area, names them the way an operator would, and turns counts into a
/// sentence — so the view is wiring and the arithmetic is testable.

import type { WorkQueue } from '../api/generated/model/workQueue.ts'

export type Area = 'enrichment' | 'subtitles' | 'discovery'

/// In the order they matter to a viewer: matching first (it names the
/// library), then subtitles (they gate a tier), then the mediahosts' own
/// discovery (it feeds both).
export const AREAS: { id: Area; label: string; intro: string }[] = [
  {
    id: 'enrichment',
    label: 'Metadata providers',
    intro: 'One queue per provider. A blocked queue is waiting on credentials or on you.',
  },
  {
    id: 'subtitles',
    label: 'Subtitles',
    intro:
      'Text tracks and image display sets are extracted on the mediahost ahead of playback; OCR reads the landed sets here, only while nobody is watching.',
  },
  {
    id: 'discovery',
    label: 'Mediahost discovery',
    intro:
      'What each mediahost still has to look at, as it last reported. It picks its own work, so there is nothing to rerun from here.',
  },
]

const QUEUE_LABELS: Record<string, string> = {
  text: 'Text extraction',
  sets: 'Display sets',
  ocr: 'Image OCR',
  scan: 'Scan',
  cheap: 'Header facts',
  hashes: 'Exact hashes',
  segments: 'Skip points',
  loudness: 'Loudness',
  local: 'Local files',
  'local-artwork': 'Local artwork',
  'tmdb-artwork': 'TMDB artwork',
  'tvdb-artwork': 'TheTVDB artwork',
  'anilist-artwork': 'AniList artwork',
  'anidb-hash': 'AniDB hashes',
  'anime-mappings': 'Anime mappings',
  'artist-collage': 'Artist collage',
  coverartarchive: 'Cover Art Archive',
  musicbrainz: 'MusicBrainz',
  theaudiodb: 'TheAudioDB',
  tmdb: 'TMDB',
  tvdb: 'TheTVDB',
  anidb: 'AniDB',
  anilist: 'AniList',
  fanart: 'Fanart.tv',
}

/// The operator's name for a queue; the raw id when it has none, because a
/// new provider must show up rather than vanish.
export function queueLabel(queue: string): string {
  return QUEUE_LABELS[queue] ?? queue
}

export interface Grouped {
  area: (typeof AREAS)[number]
  queues: WorkQueue[]
}

/// Grouped in AREAS order; an area with no queues is left out rather than
/// shown empty. Within an area, host and collection first so one mediahost's
/// rows sit together, then by queue name.
export function byArea(queues: WorkQueue[]): Grouped[] {
  return AREAS.map((area) => ({
    area,
    queues: queues
      .filter((q) => q.area === area.id)
      .sort(
        (a, b) =>
          (a.host ?? '').localeCompare(b.host ?? '') ||
          (a.collection ?? '').localeCompare(b.collection ?? '') ||
          queueLabel(a.queue).localeCompare(queueLabel(b.queue)),
      ),
  })).filter((g) => g.queues.length > 0)
}

/// How far a queue is. Discovery rows report only what is left, so they
/// have no total and no percentage — a bar with no end would be a lie.
export function progress(q: WorkQueue): { done: number; total: number; pct: number | null } {
  const total = q.pending + q.running + q.retry + q.blocked + q.done
  if (q.area === 'discovery' || total === 0) return { done: q.done, total, pct: null }
  return { done: q.done, total, pct: Math.round((q.done / total) * 100) }
}

/// What is still to do, as the counts the operator would ask about. Zero
/// counts are dropped; an idle queue says so in one word.
export function remaining(q: WorkQueue): string {
  const parts = [
    [q.running, 'running'],
    [q.pending, 'pending'],
    [q.retry, 'waiting to retry'],
    [q.blocked, 'blocked'],
  ]
    .filter(([n]) => (n as number) > 0)
    .map(([n, word]) => `${n} ${word}`)
  return parts.length ? parts.join(' · ') : 'idle'
}

/// "in 4 min", "in 2 h", or nothing when the queue is not waiting on the
/// clock. A due time in the past reads as "now": the driver is about to
/// take it, and a negative age is not information.
export function dueIn(q: WorkQueue, now: number): string | null {
  if (q.next_due == null || (q.pending === 0 && q.retry === 0 && q.running === 0)) return null
  const seconds = q.next_due - now
  if (seconds <= 0) return 'now'
  if (seconds < 90) return `in ${seconds} s`
  if (seconds < 90 * 60) return `in ${Math.round(seconds / 60)} min`
  if (seconds < 36 * 3600) return `in ${Math.round(seconds / 3600)} h`
  return `in ${Math.round(seconds / 86400)} d`
}

/// Whether the queue is in a state a rerun would change. Rerunning an idle,
/// clean queue costs a full pass for nothing, so the button stays quiet.
export function needsAttention(q: WorkQueue): boolean {
  return q.blocked > 0 || q.error != null
}
