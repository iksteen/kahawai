/// theintrodb.org: community skip timestamps, used only where the hub's own
/// detector has nothing — movies (never analysed locally) and seasons the
/// sweep has not reached. Their terms want exactly this shape: "The API is
/// intended for CLIENT SIDE USE ONLY", so the BROWSER asks and the hub never
/// proxies or stores the answers.
///
/// A bare `fetch` on purpose — `api/transport.ts` attaches the kahawai
/// bearer token, which must never reach a third party — and `no-referrer`
/// to drop the Referer header entirely. The hub's HOSTNAME still travels: a
/// cross-origin fetch is CORS mode and the Origin header cannot be
/// suppressed, which is why the settings copy discloses it. Every failure
/// mode answers an empty list: a missing skip button is the correct
/// degradation, and 404 is their normal "nobody has submitted this title".

import type { Segment } from '../api/generated/model/segment.ts'
import { cacheKey, cached, makeRoom, remember } from '../domain/introdb-cache.ts'

const BASE = 'https://api.theintrodb.org/v3/media'
const TIMEOUT_MS = 4_000
const KINDS = ['intro', 'recap', 'credits', 'preview'] as const
/// Their own documented submission ceilings, applied to spans with an
/// EXPLICIT end (a null end legitimately runs to the end of the film).
const KIND_MAX_MS: Record<(typeof KINDS)[number], number> = {
  intro: 200_000,
  recap: 1_200_000,
  credits: 1_800_000,
  preview: 1_800_000,
}
/// A body is data from a third party: however many spans it claims, no
/// title has dozens of real boundaries, and an absurd count must not be
/// sorted, stored and walked on every timeupdate.
const MAX_SPANS_PER_KIND = 24
const MAX_BODY_BYTES = 262_144

/// One promise per key, held only while the request is IN FLIGHT:
/// concurrent remounts (every session restart remounts the picture) join it
/// instead of re-sending. A settled answer lives in the cache's memo, which
/// carries the TTLs — so a TV tab that outlives them re-asks like a fresh
/// page would. A failure holds its key for `FAILURE_HOLD_MS` and then a new
/// playback may try again.
const asked = new Map<string, Promise<Segment[]>>()
const failedAt = new Map<string, number>()
const FAILURE_HOLD_MS = 10 * 60_000
/// The persisted, all-keys version of the brake — short: one probe per two
/// minutes while their server or the link is down.
const FAILURE_HOLD_SHORT_MS = 2 * 60_000

/// Their 429 answers when either the 10-second burst window or the daily
/// per-IP cap is spent. `Retry-After` is honoured when READABLE — but their
/// responses carry no Access-Control-Expose-Headers (verified live), so a
/// browser's CORS fetch cannot see it today and the default carries every
/// 429. The default is therefore MINUTES, not an hour: tripping the
/// 10-second burst window must not cost the household the feature for an
/// hour, and a day-long cap merely answers one cheap 429 per expiry.
/// Persisted, so a reload does not immediately resume.
const HOLD_KEY = 'kahawai.introdb.hold'
const HOLD_DEFAULT_MS = 10 * 60_000
/// Nothing they send may hold us longer than a day — their own daily cap's
/// window — or a bogus persisted value turns the feature off for ever.
const HOLD_MAX_MS = 86_400_000
let holdUntil = 0
try {
  holdUntil = Math.min(Number(localStorage.getItem(HOLD_KEY)) || 0, Date.now() + HOLD_MAX_MS)
} catch {
  // Storage denied: the in-memory hold still applies for this page.
}

/// Never resume faster than this, whatever Retry-After says: a fractional
/// value would defeat the hold entirely, and 429 means STOP.
const HOLD_MIN_MS = 10_000

function extendHold(until: number) {
  // EXTEND, never shrink: a 10-second burst hold answering after a
  // day-long cap hold must not overwrite it — in memory or on disk.
  holdUntil = Math.max(holdUntil, until)
  try {
    const stored = Number(localStorage.getItem(HOLD_KEY)) || 0
    if (holdUntil > stored) localStorage.setItem(HOLD_KEY, String(holdUntil))
  } catch {
    // A full origin must not cost the hold its persistence — the hold is
    // what keeps a reloading TV from hammering a spent cap. Make room at
    // the cache's expense and retry; private mode lands in the inner catch.
    try {
      makeRoom()
      localStorage.setItem(HOLD_KEY, String(holdUntil))
    } catch {
      // Page-lifetime hold only.
    }
  }
}

function holdOff(retryAfter: string | null, now = Date.now()) {
  const secs = Number(retryAfter)
  const wait =
    Number.isFinite(secs) && secs > 0
      ? Math.min(Math.max(secs * 1000, HOLD_MIN_MS), HOLD_MAX_MS)
      : HOLD_DEFAULT_MS
  extendHold(now + wait)
}

/// Read a body STOPPING at the bound: `response.text()` buffers everything
/// first, so on a chunked response (no Content-Length to pre-check) the
/// length test would run only after the damage. Environments without body
/// streams fall back to text(), which their Response implementations bound
/// by construction.
async function boundedText(response: Response): Promise<string | null> {
  const length = Number(response.headers.get('content-length'))
  if (length > MAX_BODY_BYTES) return null
  const reader = response.body?.getReader?.()
  if (!reader) {
    const text = await response.text()
    return text.length > MAX_BODY_BYTES ? null : text
  }
  const chunks: Uint8Array[] = []
  let size = 0
  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    size += value.byteLength
    if (size > MAX_BODY_BYTES) {
      void reader.cancel().catch(() => {})
      return null
    }
    chunks.push(value)
  }
  const joined = new Uint8Array(size)
  let at = 0
  for (const chunk of chunks) {
    joined.set(chunk, at)
    at += chunk.byteLength
  }
  return new TextDecoder().decode(joined)
}

type Span = { start_ms: number | null; end_ms: number | null }

/// The duration-relative span rules, shared verbatim by the wire path
/// (`normalize`) and the cache read (`withinFile`) so a span can never be
/// admitted fresh and rejected cached, or vice versa:
/// - edges inside the file, ordered;
/// - no span longer than HALF the file — the absolute per-kind ceilings do
///   not protect a short episode, where "30 minutes of credits" IS the
///   whole file and one press swallows it;
/// - a span ending at the file's edge (the to-the-end convention, or an
///   explicit end clamped there) must start in the back half.
function sane(span: Segment, durationMs: number): boolean {
  if (span.start_ms < 0 || span.start_ms >= durationMs) return false
  if (span.end_ms > durationMs || span.end_ms <= span.start_ms) return false
  if (span.end_ms - span.start_ms > durationMs / 2) return false
  if (span.end_ms === durationMs && span.start_ms < durationMs / 2) return false
  // An intro or recap TOUCHING the file's edge is junk by their own schema
  // (those kinds require an explicit end) — and it would be a mislabelled
  // "Skip intro" that ends the film, since edge-enders skip the ceilings.
  if (span.end_ms === durationMs && (span.kind === 'intro' || span.kind === 'recap')) return false
  return true
}

/// Cached entries get the same duration rules plus the interior-end
/// per-kind ceiling (the null-vs-explicit distinction is lost in storage,
/// so an edge-ending span is treated by the tail rules alone, as on the
/// wire).
function withinFile(span: Segment, durationMs: number): boolean {
  if (!sane(span, durationMs)) return false
  const max = KIND_MAX_MS[span.kind as (typeof KINDS)[number]]
  if (span.end_ms !== durationMs && max !== undefined && span.end_ms - span.start_ms > max)
    return false
  return true
}

const finiteOrNull = (v: unknown): number | null =>
  v === null ? null : typeof v === 'number' && Number.isFinite(v) ? v : NaN

/// Their nulls are meaningful — start null is "from the beginning", end null
/// is "to the end of the media" — and the player's `skippable` takes neither,
/// so they are resolved against the file here. Everything else about a span
/// is DISTRUSTED: a non-number, an Infinity or a negative edge drops the
/// span rather than minting a button that skips the whole film.
export function normalize(body: unknown, durationMs: number): Segment[] {
  if (typeof body !== 'object' || body === null) return []
  const out: Segment[] = []
  for (const kind of KINDS) {
    const spans = (body as Record<string, unknown>)[kind]
    if (!Array.isArray(spans)) continue
    for (const span of spans.slice(0, MAX_SPANS_PER_KIND) as Span[]) {
      if (typeof span !== 'object' || span === null) continue
      const start = finiteOrNull(span.start_ms)
      const end = finiteOrNull(span.end_ms)
      if (Number.isNaN(start) || Number.isNaN(end)) continue
      // Their own null rules are per kind: an intro or recap REQUIRES an
      // end (null start = from the top), credits and preview REQUIRE a
      // start (null end = to the end of the media). A null where the
      // schema demands a value is junk, and admitting it is how a
      // null-ended "intro" becomes a two-hour skip button.
      if ((kind === 'intro' || kind === 'recap') && end === null) continue
      if ((kind === 'credits' || kind === 'preview') && start === null) continue
      const start_ms = start ?? 0
      // An end past the file is clamped to it — their times are for a
      // release the duration key may only have approximated.
      const end_ms = Math.min(end ?? durationMs, durationMs)
      if (!sane({ kind, start_ms, end_ms, source: 'introdb' }, durationMs)) continue
      // An explicit interior end is additionally bound by their own
      // submission rules; anything past the kind's ceiling is a unit slip
      // or another cut's times. (A span ending at the file's edge follows
      // the tail rules in `sane` instead — `end` may have been clamped.)
      if (end_ms !== durationMs && end_ms - start_ms > KIND_MAX_MS[kind]) continue
      out.push({ kind, start_ms, end_ms, source: 'introdb' })
    }
  }
  return out.sort((a, b) => a.start_ms - b.start_ms)
}

export type IntrodbItem = {
  kind: string
  season?: number | null
  episode?: number | null
  episode_end?: number | null
  metadata?: {
    tmdb_id?: number | null
    tvdb_id?: number | null
    proj_season?: number | null
    proj_episode?: number | null
  } | null
}

/// `durationMs` is the duration of what is PLAYING (the session's), not the
/// item's minimum across renditions: it discriminates the release version on
/// their side and resolves their null-end convention on ours, so the wrong
/// one fetches the wrong cut's times. Without it there is no lookup at all —
/// a question whose answer cannot be applied is not worth quota.
export async function introdbSegments(item: IntrodbItem, durationMs: number): Promise<Segment[]> {
  if (!durationMs) return []
  // Only the kinds their database describes. A multi-episode file is out
  // too: its single answer would be E01's credits offered mid-file.
  if (item.kind !== 'movie' && item.kind !== 'episode') return []
  if (item.episode_end != null) return []
  const tv = item.kind === 'episode'
  // The provider's curated numbering outranks the file's own. Today the
  // hub only curates absolute-numbered releases (a file with no season of
  // its own); a split-cour file that names a season the provider disagrees
  // with still looks up under the file's numbering.
  const season = tv ? (item.metadata?.proj_season ?? item.season) : null
  const episode = tv ? (item.metadata?.proj_episode ?? item.episode) : null
  if (tv && (season == null || episode == null)) return []
  const key = cacheKey(
    { tmdb: item.metadata?.tmdb_id, tvdb: item.metadata?.tvdb_id },
    season,
    episode,
    durationMs,
  )
  if (!key) return [] // no usable identity — AniList and MusicBrainz land here
  const known = cached(key)
  // Re-bounded on every read: the cache validates what it CAN without a
  // duration (`valid`), and this is where the duration lives — a poisoned
  // same-origin entry must not mint the whole-film button `normalize`
  // blocks on the wire.
  if (known) return known.filter((span) => withinFile(span, durationMs))
  const pending = asked.get(key)
  if (pending) return pending
  const now = Date.now()
  const failed = failedAt.get(key)
  if (failed !== undefined && now - failed < FAILURE_HOLD_MS) return []
  // Re-read the persisted hold: another TAB may have taken the 429, and a
  // hold this tab cannot see is a hold it will spend quota against.
  try {
    const stored = Number(localStorage.getItem(HOLD_KEY)) || 0
    if (stored > now + HOLD_MAX_MS) {
      // Garbage (clock skew, a bad write): REWRITE it clamped, or clamping
      // on every read renews a rolling day-long hold for ever.
      localStorage.setItem(HOLD_KEY, String(now + HOLD_MAX_MS))
      holdUntil = Math.max(holdUntil, now + HOLD_MAX_MS)
    } else {
      holdUntil = Math.max(holdUntil, stored)
    }
  } catch {
    // Storage denied: this tab's own hold still applies.
  }
  if (now < holdUntil) return []

  const query = new URLSearchParams()
  if (item.metadata?.tmdb_id != null) query.set('tmdb_id', String(item.metadata.tmdb_id))
  else query.set('tvdb_id', String(item.metadata!.tvdb_id))
  if (tv) {
    query.set('season', String(season))
    query.set('episode', String(episode))
  }
  query.set('duration_ms', String(durationMs))

  const fail = (): Segment[] => {
    failedAt.set(key, Date.now())
    // A short persisted brake as well: `failedAt` dies with the page, and a
    // TV that reloads between titles on a dead link would otherwise re-ask
    // every title on every reload. If their server is down for one title,
    // it is down for all of them.
    extendHold(Date.now() + FAILURE_HOLD_SHORT_MS)
    asked.delete(key)
    return []
  }
  // `then` defers the body past `asked.set` below, so even a synchronous
  // throw (an old WebView without AbortSignal.timeout) settles a promise
  // that is ALREADY registered — and fail()'s delete can free the key.
  const attempt = Promise.resolve().then(async (): Promise<Segment[]> => {
    try {
      const response = await fetch(`${BASE}?${query}`, {
        signal: AbortSignal.timeout(TIMEOUT_MS),
        referrerPolicy: 'no-referrer',
        // A redirect is not theirs to issue; following one from a
        // compromised server would carry same-origin cookies wherever it
        // pointed.
        redirect: 'error',
      })
      if (response.status === 404) {
        // Only THEIR 404 is an answer ("media not found", application/json).
        // A router's, a CDN's or a retired path's 404 is an outage, and
        // negatively caching an outage kills the feature for three days per
        // title touched.
        if (!(response.headers.get('content-type') ?? '').includes('json')) return fail()
        const notFound = await boundedText(response)
        if (notFound === null) return fail()
        try {
          JSON.parse(notFound)
        } catch {
          return fail()
        }
        remember(key, []) // negative answer, cached: replays must not re-spend quota
        asked.delete(key) // the memo carries it from here, WITH its TTL
        return []
      }
      if (response.status === 429) {
        holdOff(response.headers.get('retry-after'))
        // The HOLD is the stop, and it expires; the key itself stays free
        // or this title never gets its buttons back without a reload.
        asked.delete(key)
        return []
      }
      if (!response.ok) return fail() // never negatively cached: a 502 is not an answer
      // A third party's body is read bounded: their real answers are under
      // a kilobyte, and both JSON.parse of an arbitrary document and the
      // BUFFERING of one are main-thread costs the 4 s download timeout
      // does not prevent — a chunked response carries no Content-Length,
      // so the read itself must stop at the bound.
      const text = await boundedText(response)
      if (text === null) return fail()
      const segments = normalize(JSON.parse(text) as unknown, durationMs)
      remember(key, segments)
      asked.delete(key) // the memo carries it from here, WITH its TTL
      return segments
    } catch {
      return fail()
    }
  })
  asked.set(key, attempt)
  return attempt
}

/// For tests: the in-memory hold alone, leaving storage as another tab
/// would see it.
export function resetIntrodbHoldOnly() {
  holdUntil = 0
  asked.clear()
  failedAt.clear()
}

/// For tests.
export function resetIntrodb() {
  asked.clear()
  failedAt.clear()
  holdUntil = 0
  try {
    localStorage.removeItem(HOLD_KEY)
  } catch {
    // nothing held
  }
}
