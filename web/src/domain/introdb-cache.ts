/// A localStorage cache for theintrodb answers, and the reason it exists:
/// their unauthenticated `/media` is capped at 500 requests per DAY per IP,
/// shared by every viewer behind one router. Without a cache, replays spend
/// quota; without negative caching, every replay of an UNKNOWN title spends
/// quota for ever. So both answers are stored — hits for a month (community
/// timestamps move slowly), misses for a few days (a newly-submitted one
/// should appear without anyone clearing storage).

import type { Segment } from '../api/generated/model/segment.ts'

const PREFIX = 'kahawai.introdb.v1:'
const HIT_TTL_MS = 30 * 24 * 3_600_000
const MISS_TTL_MS = 3 * 24 * 3_600_000
/// A big library must not crowd the origin's storage quota; oldest go
/// first. Sized for episode-heavy libraries — a season is ~12 keys — since
/// every eviction is a future re-spend against their daily cap.
const MAX_ENTRIES = 2000

type Entry = { at: number; segments: Segment[] }

/// Persisted data is as untrusted as the network's: anything same-origin can
/// write these keys, and a stale bundle can leave an older shape behind.
/// This gate enforces what it can WITHOUT the duration — kinds from the
/// known set, sane finite ordered edges, a global length ceiling — and the
/// caller re-bounds every span against the playing file (`withinFile` in
/// api/introdb.ts), which is what blocks the whole-film skip button.
const CACHE_KINDS = new Set(['intro', 'recap', 'credits', 'preview'])
/// Their global ceiling (6 h); per-kind precision lives in `normalize`,
/// which has the duration in hand — this gate does not.
const MAX_SPAN_MS = 21_600_000

const valid = (s: Segment) =>
  typeof s === 'object' &&
  s !== null &&
  typeof s.kind === 'string' &&
  CACHE_KINDS.has(s.kind) &&
  s.source === 'introdb' &&
  Number.isFinite(s.start_ms) &&
  Number.isFinite(s.end_ms) &&
  s.start_ms >= 0 &&
  s.end_ms > s.start_ms &&
  s.end_ms - s.start_ms <= MAX_SPAN_MS

/// A rewatch in the same session touches neither storage nor the wire.
/// Entries carry their timestamp, and the in-flight map in api/introdb.ts
/// frees each key once the answer lands here — so a TV tab that lives past
/// the TTLs re-asks like a fresh page would, instead of a title unknown on
/// Monday staying unknown for the life of the tab.
const memo = new Map<string, Entry>()

export function cacheKey(
  keyed: { tmdb?: number | null | undefined; tvdb?: number | null | undefined },
  season: number | null | undefined,
  episode: number | null | undefined,
  durationMs: number | null | undefined,
): string | null {
  const id =
    keyed.tmdb != null ? `tmdb:${keyed.tmdb}` : keyed.tvdb != null ? `tvdb:${keyed.tvdb}` : null
  if (!id) return null
  const se = season != null && episode != null ? `:s${season}e${episode}` : ''
  const d = durationMs ? `:d${durationMs}` : ''
  return `${PREFIX}${id}${se}${d}`
}

export function cached(key: string, now = Date.now()): Segment[] | null {
  const hot = memo.get(key)
  if (hot && now - hot.at <= (hot.segments.length ? HIT_TTL_MS : MISS_TTL_MS)) return hot.segments
  try {
    const raw = localStorage.getItem(key)
    if (!raw) return null
    const entry = JSON.parse(raw) as Entry
    // A poisoned or truncated entry must cost a re-fetch, not a render
    // crash: `skippable` walks these on every timeupdate.
    if (
      typeof entry.at !== 'number' ||
      !Number.isFinite(entry.at) ||
      // A future-dated entry never expires and never evicts: clock skew
      // writes them innocently, and they must not survive the correction.
      entry.at > now + 60_000 ||
      !Array.isArray(entry.segments) ||
      // Bounded, as the wire path is: 100k individually-valid spans is a
      // main-thread walk on every timeupdate, not an answer.
      entry.segments.length > 96 ||
      !entry.segments.every(valid)
    ) {
      localStorage.removeItem(key)
      return null
    }
    const ttl = entry.segments.length ? HIT_TTL_MS : MISS_TTL_MS
    if (now - entry.at > ttl) {
      localStorage.removeItem(key)
      return null
    }
    memo.set(key, entry)
    return entry.segments
  } catch {
    // Unparseable, or storage denied: ask again.
    return null
  }
}

export function remember(key: string, segments: Segment[], now = Date.now()) {
  memo.set(key, { at: now, segments })
  const value = JSON.stringify({ at: now, segments } satisfies Entry)
  try {
    sweep()
    localStorage.setItem(key, value)
  } catch {
    // The count-gated sweep cannot help when the ORIGIN's quota is full at
    // fewer than MAX_ENTRIES — big entries, or another feature's data. A
    // cache that never evicts once full is wedged for good, and every
    // reload then re-spends the daily request cap; so evict by force and
    // retry once. Private mode still lands in the inner catch, memo-only.
    try {
      makeRoom()
      localStorage.setItem(key, value)
    } catch {
      // The memo carries this page's lifetime.
    }
  }
}

/// Evict the oldest entries unconditionally — the quota-pressure escape
/// hatch, where the count bound has already failed to help.
export function makeRoom() {
  let mine = 0
  for (let n = 0; n < localStorage.length; n++) {
    if (localStorage.key(n)?.startsWith(TRIM_PREFIX)) mine++
  }
  sweep(Math.max(0, mine - 64))
}

/// Every generation, not just the current one: a version bump must not
/// strand the old entries in the origin's quota for ever.
const TRIM_PREFIX = 'kahawai.introdb.v'

/// Exported for tests: driving eviction through `remember` at the real
/// bound would be quadratic in MAX_ENTRIES.
export function sweep(keep: number = MAX_ENTRIES - 1) {
  // Snapshot the keys first: removing while iterating by index re-indexes
  // the store and skips the entry after every removal.
  const keys: string[] = []
  for (let n = 0; n < localStorage.length; n++) {
    const key = localStorage.key(n)
    if (key?.startsWith(TRIM_PREFIX)) keys.push(key)
  }
  const now = Date.now()
  const mine: { key: string; at: number }[] = []
  for (const key of keys) {
    try {
      const at = (JSON.parse(localStorage.getItem(key)!) as Entry).at
      // Unreadable or future-dated sorts as oldest, so junk evicts first.
      mine.push({ key, at: typeof at === 'number' && at <= now ? at : 0 })
    } catch {
      localStorage.removeItem(key)
    }
  }
  if (mine.length <= keep) return
  mine.sort((a, b) => a.at - b.at)
  for (const { key } of mine.slice(0, mine.length - keep)) localStorage.removeItem(key)
}

/// For tests.
export function forgetIntrodb() {
  memo.clear()
}
