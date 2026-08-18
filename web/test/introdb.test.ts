/// theintrodb is quota-bound — 500 unauthenticated requests per day per IP,
/// shared by the whole household — so what these tests pin is mostly about
/// NOT asking: cached hits, cached misses, in-flight joining, the hold a
/// 429 imposes, and the page-load stop after any failure.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

import {
  introdbSegments,
  normalize,
  resetIntrodb,
  resetIntrodbHoldOnly,
} from '../src/api/introdb.ts'
import { forgetIntrodb, sweep } from '../src/domain/introdb-cache.ts'

const DUR = 7_200_000

const film = (over: Record<string, unknown> = {}) => ({
  kind: 'movie',
  season: null,
  episode: null,
  episode_end: null,
  metadata: { tmdb_id: 949, tvdb_id: null, proj_season: null, proj_episode: null },
  ...over,
})

const answer = (body: unknown, status = 200, headers: Record<string, string> = {}) =>
  vi.fn(async (_url: RequestInfo | URL, _init?: RequestInit) => {
    void _url
    void _init
    return new Response(JSON.stringify(body), {
      status,
      headers: { 'content-type': 'application/json', ...headers },
    })
  })

beforeEach(() => {
  localStorage.clear()
  forgetIntrodb()
  resetIntrodb()
})
afterEach(() => {
  vi.unstubAllGlobals()
  localStorage.clear()
  forgetIntrodb()
  resetIntrodb()
})

describe('normalising their answer', () => {
  test('nulls mean the edges of the file, and preview is carried', () => {
    const got = normalize(
      {
        intro: [{ start_ms: null, end_ms: 23_000 }],
        credits: [{ start_ms: 6_408_000, end_ms: null }],
        preview: [{ start_ms: 1_000, end_ms: 61_000 }],
      },
      DUR,
    )
    expect(got).toEqual([
      { kind: 'intro', start_ms: 0, end_ms: 23_000, source: 'introdb' },
      { kind: 'preview', start_ms: 1_000, end_ms: 61_000, source: 'introdb' },
      { kind: 'credits', start_ms: 6_408_000, end_ms: DUR, source: 'introdb' },
    ])
  })

  test('third-party spans are distrusted', () => {
    // Infinity would mint a button that skips the whole film; strings and
    // negatives are not times; a non-object body is not an answer.
    expect(normalize({ intro: [{ start_ms: 0, end_ms: Infinity }] }, DUR)).toEqual([])
    expect(normalize({ intro: [{ start_ms: '10', end_ms: '200' }] }, DUR)).toEqual([])
    expect(normalize({ intro: [{ start_ms: -5, end_ms: 200 }] }, DUR)).toEqual([])
    expect(normalize({ intro: [null, { start_ms: 0, end_ms: 20_000 }] }, DUR)).toHaveLength(1)
    expect(normalize(null, DUR)).toEqual([])
    expect(normalize([], DUR)).toEqual([])
    // A MISSING field is not their schema (fields are present-but-nullable),
    // so it is treated as junk and dropped — the judgement pinned here.
    expect(normalize({ intro: [{ start_ms: 0 }] }, DUR)).toEqual([])
    // Credits and preview REQUIRE a start in their schema: a null one would
    // mint a "Skip credits" button at the top of the film.
    expect(normalize({ credits: [{ start_ms: null, end_ms: 60_000 }] }, DUR)).toEqual([])
    expect(normalize({ preview: [{ start_ms: null, end_ms: 60_000 }] }, DUR)).toEqual([])
  })

  test('no span may swallow the film', () => {
    // Both edges null is LEGAL in their schema and resolves to the whole
    // file: pressing that button ends the movie.
    expect(normalize({ intro: [{ start_ms: null, end_ms: null }] }, DUR)).toEqual([])
    // An end past the file clamps to it; from zero that is the whole film.
    expect(normalize({ credits: [{ start_ms: 0, end_ms: 9e15 }] }, DUR)).toEqual([])
    // Clamped but partial survives.
    expect(normalize({ credits: [{ start_ms: 6_000_000, end_ms: 9e15 }] }, DUR)).toEqual([
      { kind: 'credits', start_ms: 6_000_000, end_ms: DUR, source: 'introdb' },
    ])
    // An explicit end is bound by their own per-kind ceiling: a 20-minute
    // "intro" is a unit slip or another cut's times.
    expect(normalize({ intro: [{ start_ms: 0, end_ms: 1_200_000 }] }, DUR)).toEqual([])
    // A start past the file cannot be offered either.
    expect(normalize({ credits: [{ start_ms: DUR + 1, end_ms: null }] }, DUR)).toEqual([])
  })

  test('a short episode is protected too', () => {
    // The absolute per-kind ceilings do not protect a 24-minute episode:
    // "30 minutes of credits" IS the whole file there. No span may exceed
    // half the file, fresh or cached.
    const EP = 1_440_000
    expect(normalize({ credits: [{ start_ms: 0, end_ms: 9e15 }] }, EP)).toEqual([])
    expect(normalize({ credits: [{ start_ms: 10_000, end_ms: 1_430_000 }] }, EP)).toEqual([])
    expect(normalize({ recap: [{ start_ms: 0, end_ms: 1_200_000 }] }, EP)).toEqual([])
    // The real shape survives: an ED in the last two minutes.
    expect(normalize({ credits: [{ start_ms: 1_320_000, end_ms: null }] }, EP)).toEqual([
      { kind: 'credits', start_ms: 1_320_000, end_ms: EP, source: 'introdb' },
    ])
    // An "intro" ending at the file's edge is junk by their own schema and
    // would end the film on one press — dropped whatever its start.
    expect(normalize({ intro: [{ start_ms: EP / 2, end_ms: 9e15 }] }, EP)).toEqual([])
    expect(normalize({ recap: [{ start_ms: EP / 2, end_ms: EP }] }, EP)).toEqual([])
  })

  test('an absurd span count is clipped', () => {
    const spans = Array.from({ length: 10_000 }, (_, n) => ({
      start_ms: n * 2,
      end_ms: n * 2 + 1,
    }))
    expect(normalize({ intro: spans }, DUR).length).toBeLessThanOrEqual(24)
  })
})

describe('what spends quota', () => {
  test('an answer is fetched once and replays read the cache', async () => {
    const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
    vi.stubGlobal('fetch', wire)
    const first = await introdbSegments(film(), DUR)
    expect(first).toHaveLength(1)
    forgetIntrodb() // a later page load: the memo is gone, localStorage is not
    resetIntrodb()
    const again = await introdbSegments(film(), DUR)
    expect(again).toEqual(first)
    expect(wire).toHaveBeenCalledTimes(1)
  })

  test('a 404 is an answer too, and is not re-asked', async () => {
    const wire = answer({ error: 'media not found' }, 404)
    vi.stubGlobal('fetch', wire)
    expect(await introdbSegments(film(), DUR)).toEqual([])
    forgetIntrodb()
    resetIntrodb()
    expect(await introdbSegments(film(), DUR)).toEqual([])
    expect(wire).toHaveBeenCalledTimes(1)
  })

  test('a 404 that is not THEIRS is an outage, not an answer', async () => {
    // A CDN's or a retired path's 404 is HTML; caching it for three days
    // per title would kill the feature over an infrastructure hiccup.
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async (_url: RequestInfo | URL) =>
          new Response('<html>gateway not found</html>', {
            status: 404,
            headers: { 'content-type': 'text/html' },
          }),
      ),
    )
    expect(await introdbSegments(film(), DUR)).toEqual([])
    expect(Object.keys(localStorage).filter((k) => k.startsWith('kahawai.introdb.v1'))).toEqual([])
  })

  test('concurrent lookups for one key join a single request', async () => {
    const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
    vi.stubGlobal('fetch', wire)
    const [a, b] = await Promise.all([introdbSegments(film(), DUR), introdbSegments(film(), DUR)])
    expect(a).toEqual(b)
    expect(wire).toHaveBeenCalledTimes(1)
  })

  test('a stale entry is re-asked, an unexpired one is not', async () => {
    const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
    vi.stubGlobal('fetch', wire)
    await introdbSegments(film(), DUR)
    // Age the stored entry past the miss TTL but inside the hit TTL: a HIT
    // must survive, so nothing is re-fetched.
    const key = Object.keys(localStorage).find((k) => k.startsWith('kahawai.introdb.v1'))!
    const entry = JSON.parse(localStorage.getItem(key)!)
    entry.at = Date.now() - 5 * 24 * 3_600_000
    localStorage.setItem(key, JSON.stringify(entry))
    forgetIntrodb()
    resetIntrodb()
    await introdbSegments(film(), DUR)
    expect(wire).toHaveBeenCalledTimes(1)
    // The same age on a MISS is stale and is re-asked.
    entry.segments = []
    localStorage.setItem(key, JSON.stringify(entry))
    forgetIntrodb()
    resetIntrodb()
    await introdbSegments(film(), DUR)
    expect(wire).toHaveBeenCalledTimes(2)
  })

  test('a poisoned cache entry costs a re-fetch, never a crash', async () => {
    const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
    vi.stubGlobal('fetch', wire)
    await introdbSegments(film(), DUR)
    const key = Object.keys(localStorage).find((k) => k.startsWith('kahawai.introdb.v1'))!
    localStorage.setItem(key, JSON.stringify({ at: Date.now(), segments: 'xx' }))
    forgetIntrodb()
    resetIntrodb()
    expect(await introdbSegments(film(), DUR)).toHaveLength(1)
    expect(wire).toHaveBeenCalledTimes(2)
  })

  test('a 429 holds every lookup for the Retry-After window, persisted', async () => {
    const before = Date.now()
    const wire = answer({ error: 'rate limited' }, 429, { 'retry-after': '900' })
    vi.stubGlobal('fetch', wire)
    expect(await introdbSegments(film(), DUR)).toEqual([])
    const other = film({ metadata: { tmdb_id: 550, tvdb_id: null } })
    expect(await introdbSegments(other as never, DUR)).toEqual([])
    expect(wire).toHaveBeenCalledTimes(1)
    // The WINDOW is theirs — 900s, distinct from the 600s default, so this
    // fails if the header stops being honoured. (In a real browser the
    // header is CORS-invisible today; honouring it is for the day they
    // expose it.)
    const hold = Number(localStorage.getItem('kahawai.introdb.hold'))
    expect(hold).toBeGreaterThanOrEqual(before + 900_000)
    expect(hold).toBeLessThan(Date.now() + 901_000)
  })

  test('a fractional Retry-After cannot defeat the hold', async () => {
    const wire = answer({ error: 'rate limited' }, 429, { 'retry-after': '0.5' })
    vi.stubGlobal('fetch', wire)
    await introdbSegments(film(), DUR)
    expect(Number(localStorage.getItem('kahawai.introdb.hold'))).toBeGreaterThanOrEqual(
      Date.now() + 9_000,
    )
  })

  test('an absurd Retry-After cannot disable the feature for ever', async () => {
    const wire = answer({ error: 'rate limited' }, 429, { 'retry-after': '1e308' })
    vi.stubGlobal('fetch', wire)
    await introdbSegments(film(), DUR)
    const hold = Number(localStorage.getItem('kahawai.introdb.hold'))
    expect(Number.isFinite(hold)).toBe(true)
    expect(hold).toBeLessThanOrEqual(Date.now() + 86_400_000)
  })

  test('any failure is attempted once per page load, then retried fresh', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async (_url: RequestInfo | URL) => {
        throw new TypeError('Failed to fetch')
      }),
    )
    expect(await introdbSegments(film(), DUR)).toEqual([])
    // Same page load, same key: the failed attempt is held, not repeated.
    const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
    vi.stubGlobal('fetch', wire)
    expect(await introdbSegments(film(), DUR)).toEqual([])
    expect(wire).not.toHaveBeenCalled()
    // A new page load retries.
    resetIntrodb()
    expect(await introdbSegments(film(), DUR)).toHaveLength(1)
  })

  test('no identity, no duration, or the wrong shape: no request', async () => {
    const wire = answer({})
    vi.stubGlobal('fetch', wire)
    expect(await introdbSegments(film({ metadata: null }), DUR)).toEqual([])
    expect(
      await introdbSegments(film({ metadata: { tmdb_id: null, tvdb_id: null } }) as never, DUR),
    ).toEqual([])
    expect(await introdbSegments(film(), 0)).toEqual([])
    // An episode missing its numbers, a multi-episode file, and a kind
    // their database does not describe are all unaskable.
    expect(await introdbSegments(film({ kind: 'episode', season: 1, episode: null }), DUR)).toEqual(
      [],
    )
    expect(
      await introdbSegments(film({ kind: 'episode', season: 1, episode: 1, episode_end: 2 }), DUR),
    ).toEqual([])
    expect(await introdbSegments(film({ kind: 'track' }), DUR)).toEqual([])
    expect(wire).not.toHaveBeenCalled()
  })
})

describe('the request itself', () => {
  test('a TV lookup keys on the show, preferring the curated numbering', async () => {
    const wire = answer({ intro: [{ start_ms: 0, end_ms: 10_000 }] })
    vi.stubGlobal('fetch', wire)
    await introdbSegments(
      film({
        kind: 'episode',
        season: 1,
        episode: 13,
        metadata: { tmdb_id: 1403, tvdb_id: null, proj_season: 2, proj_episode: 1 },
      }),
      2_700_000,
    )
    const url = String(wire.mock.calls[0]![0])
    expect(url).toContain('tmdb_id=1403')
    expect(url).toContain('season=2')
    expect(url).toContain('episode=1')
    expect(url).toContain('duration_ms=2700000')
  })

  test('falls back to tvdb when tmdb is absent', async () => {
    const wire = answer({ intro: [{ start_ms: 0, end_ms: 10_000 }] })
    vi.stubGlobal('fetch', wire)
    await introdbSegments(
      film({
        kind: 'episode',
        season: 1,
        episode: 1,
        metadata: { tmdb_id: null, tvdb_id: 81189, proj_season: null, proj_episode: null },
      }),
      2_700_000,
    )
    expect(String(wire.mock.calls[0]![0])).toContain('tvdb_id=81189')
  })

  test('carries no kahawai credentials and no referrer', async () => {
    const wire = answer({})
    vi.stubGlobal('fetch', wire)
    await introdbSegments(film(), DUR)
    const init = wire.mock.calls[0]![1] as RequestInit
    expect(init.referrerPolicy).toBe('no-referrer')
    // Strictly ABSENT — an empty-looking Headers instance would deep-equal
    // an empty object and hide an added header.
    expect(init.headers).toBeUndefined()
    expect(init.credentials).toBeUndefined()
    // A redirect is not theirs to issue, and a hung request must die at
    // the deadline rather than pin its key.
    expect(init.redirect).toBe('error')
    expect(init.signal).toBeInstanceOf(AbortSignal)
  })
})

describe('what survives in storage', () => {
  test('a poisoned ELEMENT is rejected, not walked by the player', async () => {
    const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
    vi.stubGlobal('fetch', wire)
    await introdbSegments(film(), DUR)
    const key = Object.keys(localStorage).find((k) => k.startsWith('kahawai.introdb.v1'))!
    localStorage.setItem(
      key,
      JSON.stringify({
        at: Date.now(),
        segments: [{ kind: 'credits', start_ms: -1e15, end_ms: 1e15, source: 'introdb' }],
      }),
    )
    forgetIntrodb()
    resetIntrodb()
    // The entry is discarded and the wire is asked again.
    expect(await introdbSegments(film(), DUR)).toHaveLength(1)
    expect(wire).toHaveBeenCalledTimes(2)
  })

  test('a future-dated entry does not outlive the clock correction', async () => {
    const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
    vi.stubGlobal('fetch', wire)
    await introdbSegments(film(), DUR)
    const key = Object.keys(localStorage).find((k) => k.startsWith('kahawai.introdb.v1'))!
    const entry = JSON.parse(localStorage.getItem(key)!)
    entry.at = Date.now() + 9 * 365 * 24 * 3_600_000 // written under 2035 skew
    localStorage.setItem(key, JSON.stringify(entry))
    forgetIntrodb()
    resetIntrodb()
    await introdbSegments(film(), DUR)
    expect(wire).toHaveBeenCalledTimes(2)
  })

  test('the oldest entries are the ones evicted, old generations first', () => {
    const now = Date.now()
    // A stale bundle's format and the rate-limit hold sit beside the cache.
    // The old generation is not swept on sight — it AGES OUT: nothing
    // refreshes its `at`, so it lands among the oldest and evicts first.
    localStorage.setItem(
      'kahawai.introdb.v0:tmdb:9:d1',
      JSON.stringify({ at: now - 30 * 24 * 3_600_000, segments: [] }),
    )
    localStorage.setItem('kahawai.introdb.hold', String(now - 1))
    for (let n = 0; n < 502; n++) {
      localStorage.setItem(
        `kahawai.introdb.v1:tmdb:${n}:d1`,
        JSON.stringify({ at: now - (502 - n) * 1000, segments: [] }),
      )
    }
    sweep(500)
    const left = Object.keys(localStorage).filter((k) => k.startsWith('kahawai.introdb.'))
    // The newest survived; the oldest and the dead generation did not; the
    // hold is not the cache's to sweep.
    expect(left).toContain('kahawai.introdb.v1:tmdb:501:d1')
    expect(left).not.toContain('kahawai.introdb.v1:tmdb:0:d1')
    expect(left).not.toContain('kahawai.introdb.v0:tmdb:9:d1')
    expect(left).toContain('kahawai.introdb.hold')
    expect(left.filter((k) => k.startsWith('kahawai.introdb.v')).length).toBeLessThanOrEqual(500)
  })

  test('a 429 does not pin its own title once the hold expires', async () => {
    vi.useFakeTimers()
    try {
      vi.stubGlobal(
        'fetch',
        vi.fn(
          async (_url: RequestInfo | URL) =>
            new Response('slow down', { status: 429, headers: { 'retry-after': '10' } }),
        ),
      )
      expect(await introdbSegments(film(), DUR)).toEqual([])
      vi.advanceTimersByTime(11_000)
      const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
      vi.stubGlobal('fetch', wire)
      // The SAME title asks again after the hold — it was never memoised.
      expect(await introdbSegments(film(), DUR)).toHaveLength(1)
    } finally {
      vi.useRealTimers()
    }
  })

  test('a transient failure is neither persisted nor permanent', async () => {
    vi.useFakeTimers()
    try {
      vi.stubGlobal(
        'fetch',
        vi.fn(async (_url: RequestInfo | URL) => new Response('bad gateway', { status: 502 })),
      )
      expect(await introdbSegments(film(), DUR)).toEqual([])
      // Not negatively cached: a 502 is not an answer.
      expect(Object.keys(localStorage).filter((k) => k.startsWith('kahawai.introdb.v1'))).toEqual(
        [],
      )
      // Held for the failure window...
      const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
      vi.stubGlobal('fetch', wire)
      expect(await introdbSegments(film(), DUR)).toEqual([])
      expect(wire).not.toHaveBeenCalled()
      // ...and free again after it, without a page reload.
      vi.advanceTimersByTime(11 * 60_000)
      expect(await introdbSegments(film(), DUR)).toHaveLength(1)
    } finally {
      vi.useRealTimers()
    }
  })
})

describe('what the cache refuses', () => {
  const seed = (segments: unknown, at: unknown = Date.now()) =>
    localStorage.setItem('kahawai.introdb.v1:tmdb:949:d7200000', JSON.stringify({ at, segments }))

  test('per-kind ceilings hold on the cache path too', async () => {
    // Passes `sane` (5 min of a 2h film) but not the intro ceiling: without
    // the withinFile ceiling a poisoned entry re-opens on replay what
    // normalize blocks on the wire.
    seed([{ kind: 'intro', start_ms: 0, end_ms: 300_000, source: 'introdb' }])
    const wire = answer({})
    vi.stubGlobal('fetch', wire)
    expect(await introdbSegments(film(), DUR)).toEqual([])
    expect(wire).not.toHaveBeenCalled()
  })

  test('a timestamp that is not a time discards the entry', async () => {
    // NaN comparisons make a non-numeric `at` immortal without the type
    // gate: never stale, never evicted.
    seed([{ kind: 'intro', start_ms: 0, end_ms: 30_000, source: 'introdb' }], 'x')
    const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
    vi.stubGlobal('fetch', wire)
    expect(await introdbSegments(film(), DUR)).toHaveLength(1)
    expect(wire).toHaveBeenCalledTimes(1)
  })

  test('unknown kinds, foreign sources and absurd counts are refused', async () => {
    const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
    vi.stubGlobal('fetch', wire)
    for (const segments of [
      [{ kind: 'commercial', start_ms: 0, end_ms: 30_000, source: 'introdb' }],
      [{ kind: 'intro', start_ms: 0, end_ms: 30_000, source: 'somewhere-else' }],
      Array.from({ length: 97 }, (_, n) => ({
        kind: 'intro',
        start_ms: n * 2,
        end_ms: n * 2 + 1,
        source: 'introdb',
      })),
    ]) {
      localStorage.clear()
      forgetIntrodb()
      resetIntrodb()
      seed(segments)
      // The EXACT wire answer, or an unlabelled kind / foreign-attributed
      // span served from the poisoned entry would count as the one span too.
      expect(await introdbSegments(film(), DUR)).toEqual([
        { kind: 'intro', start_ms: 0, end_ms: 30_000, source: 'introdb' },
      ])
    }
  })

  test('a settled answer expires like a cache entry, not with the tab', async () => {
    // The in-flight map frees the key once the memo holds the answer; age
    // the memo past the miss TTL and the SAME tab re-asks — a TV tab lives
    // for weeks and Monday's 404 must not outlive Monday by much.
    vi.useFakeTimers()
    try {
      const miss = answer({ error: 'media not found' }, 404)
      vi.stubGlobal('fetch', miss)
      expect(await introdbSegments(film(), DUR)).toEqual([])
      vi.advanceTimersByTime(4 * 24 * 3_600_000)
      const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
      vi.stubGlobal('fetch', wire)
      expect(await introdbSegments(film(), DUR)).toHaveLength(1)
    } finally {
      vi.useRealTimers()
    }
  })
})

describe('a full origin', () => {
  test('a quota error evicts and retries rather than wedging the cache', async () => {
    // On the INSTANCE: happy-dom binds `setItem` onto localStorage at first
    // access, so a prototype spy patches an object nothing reads.
    const real = localStorage.setItem.bind(localStorage)
    let threw = 0
    const spy = vi.spyOn(localStorage, 'setItem').mockImplementation((key, value) => {
      // The first cache write hits a full origin; after eviction it works.
      if (key.startsWith('kahawai.introdb.v1') && threw === 0) {
        threw++
        throw new DOMException('quota', 'QuotaExceededError')
      }
      real(key, value)
    })
    try {
      const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
      vi.stubGlobal('fetch', wire)
      expect(await introdbSegments(film(), DUR)).toHaveLength(1)
      // Persisted despite the quota error: the retry landed.
      expect(localStorage.getItem('kahawai.introdb.v1:tmdb:949:d7200000')).not.toBeNull()
    } finally {
      spy.mockRestore()
    }
  })

  test('the 429 hold survives a full origin the same way', async () => {
    const real = localStorage.setItem.bind(localStorage)
    let threw = 0
    const spy = vi.spyOn(localStorage, 'setItem').mockImplementation((key, value) => {
      if (key === 'kahawai.introdb.hold' && threw === 0) {
        threw++
        throw new DOMException('quota', 'QuotaExceededError')
      }
      real(key, value)
    })
    try {
      vi.stubGlobal(
        'fetch',
        vi.fn(async (_url: RequestInfo | URL) => new Response('', { status: 429 })),
      )
      await introdbSegments(film(), DUR)
      expect(Number(localStorage.getItem('kahawai.introdb.hold'))).toBeGreaterThan(Date.now())
    } finally {
      spy.mockRestore()
    }
  })
})

describe('the hold across tabs and time', () => {
  test('a hold another tab persisted is honoured here', async () => {
    // resetIntrodb (in beforeEach) cleared everything; the OTHER tab's 429
    // arrives as a bare localStorage write this module never saw.
    localStorage.setItem('kahawai.introdb.hold', String(Date.now() + 600_000))
    const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
    vi.stubGlobal('fetch', wire)
    expect(await introdbSegments(film(), DUR)).toEqual([])
    expect(wire).not.toHaveBeenCalled()
  })

  test('a persisted far-future hold expires within a day', async () => {
    vi.useFakeTimers()
    try {
      localStorage.setItem('kahawai.introdb.hold', '1e18')
      const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
      vi.stubGlobal('fetch', wire)
      expect(await introdbSegments(film(), DUR)).toEqual([])
      vi.advanceTimersByTime(25 * 3_600_000)
      expect(await introdbSegments(film(), DUR)).toHaveLength(1)
    } finally {
      vi.useRealTimers()
    }
  })

  test('a short burst hold cannot shorten a standing day hold', async () => {
    // GENUINELY concurrent: both requests must be in flight before either
    // 429 lands, or the second lookup reads the first hold and never
    // reaches the code under test. The day-cap answer resolves first, the
    // burst answer second — the persisted hold must stay the long one.
    let settleDay!: (r: Response) => void
    let settleBurst!: (r: Response) => void
    const wires = [
      new Promise<Response>((r) => (settleDay = r)),
      new Promise<Response>((r) => (settleBurst = r)),
    ]
    let n = 0
    vi.stubGlobal(
      'fetch',
      vi.fn((_url: RequestInfo | URL) => wires[n++]),
    )
    const day = introdbSegments(film(), DUR)
    const burst = introdbSegments(film({ metadata: { tmdb_id: 550, tvdb_id: null } }), DUR)
    settleDay(new Response('', { status: 429, headers: { 'retry-after': '3600' } }))
    await day
    const standing = Number(localStorage.getItem('kahawai.introdb.hold'))
    expect(standing).toBeGreaterThanOrEqual(Date.now() + 3_500_000)
    settleBurst(new Response('', { status: 429, headers: { 'retry-after': '10' } }))
    await burst
    expect(Number(localStorage.getItem('kahawai.introdb.hold'))).toBeGreaterThanOrEqual(standing)
  })

  test("a burst 429 in flight cannot shrink another tab's day hold on disk", async () => {
    // The request passes its hold re-read while storage is empty; ANOTHER
    // tab then persists an hour-long day-cap hold; our burst 429 lands
    // after it. The disk write must only raise.
    let settle!: (r: Response) => void
    vi.stubGlobal(
      'fetch',
      vi.fn((_url: RequestInfo | URL) => new Promise<Response>((r) => (settle = r))),
    )
    const lookup = introdbSegments(film(), DUR)
    // One tick so the deferred body calls fetch and `settle` exists — the
    // hold re-read already ran synchronously, before any storage write.
    await Promise.resolve()
    const otherTabs = Date.now() + 3_600_000
    localStorage.setItem('kahawai.introdb.hold', String(otherTabs))
    settle(new Response('', { status: 429, headers: { 'retry-after': '10' } }))
    expect(await lookup).toEqual([])
    expect(Number(localStorage.getItem('kahawai.introdb.hold'))).toBeGreaterThanOrEqual(otherTabs)
  })

  test('a network failure brakes every key briefly, across reloads', async () => {
    vi.useFakeTimers()
    try {
      vi.stubGlobal(
        'fetch',
        vi.fn(async (_url: RequestInfo | URL) => {
          throw new TypeError('Failed to fetch')
        }),
      )
      expect(await introdbSegments(film(), DUR)).toEqual([])
      // A different key on a simulated reload — module memos AND the
      // in-memory hold gone, only storage surviving.
      forgetIntrodb()
      resetIntrodbHoldOnly()
      const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
      vi.stubGlobal('fetch', wire)
      expect(
        await introdbSegments(film({ metadata: { tmdb_id: 550, tvdb_id: null } }), DUR),
      ).toEqual([])
      expect(wire).not.toHaveBeenCalled()
      vi.advanceTimersByTime(3 * 60_000)
      expect(
        await introdbSegments(film({ metadata: { tmdb_id: 550, tvdb_id: null } }), DUR),
      ).toHaveLength(1)
    } finally {
      vi.useRealTimers()
    }
  })
})

describe('bounded bodies and bounded cache reads', () => {
  test('a fetch that throws synchronously does not pin its key', async () => {
    // An old WebView without AbortSignal.timeout throws before the first
    // await; the key must still be registered-then-freed, or that title
    // never recovers without a reload.
    vi.stubGlobal(
      'fetch',
      vi.fn((_url: RequestInfo | URL): Promise<Response> => {
        throw new TypeError('AbortSignal.timeout is not a function')
      }),
    )
    vi.useFakeTimers()
    try {
      expect(await introdbSegments(film(), DUR)).toEqual([])
      vi.advanceTimersByTime(11 * 60_000)
      const wire = answer({ intro: [{ start_ms: 0, end_ms: 30_000 }] })
      vi.stubGlobal('fetch', wire)
      expect(await introdbSegments(film(), DUR)).toHaveLength(1)
    } finally {
      vi.useRealTimers()
    }
  })

  test('an oversized 404 is an outage even in JSON clothing', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async (_url: RequestInfo | URL) =>
          new Response('[' + '"x",'.repeat(120_000).slice(0, -1) + ']', {
            status: 404,
            headers: { 'content-type': 'application/json' },
          }),
      ),
    )
    expect(await introdbSegments(film(), DUR)).toEqual([])
    expect(Object.keys(localStorage).filter((k) => k.startsWith('kahawai.introdb.v1'))).toEqual([])
  })

  test('an oversized body is a failure, not a parse', async () => {
    const big = '{"intro":[' + '{"start_ms":0,"end_ms":1},'.repeat(20_000).slice(0, -1) + ']}'
    vi.stubGlobal(
      'fetch',
      vi.fn(async (_url: RequestInfo | URL) => new Response(big, { status: 200 })),
    )
    expect(await introdbSegments(film(), DUR)).toEqual([])
    // Not an answer: nothing cached.
    expect(Object.keys(localStorage).filter((k) => k.startsWith('kahawai.introdb.v1'))).toEqual([])
  })

  test('a cached span past the playing file is not offered', async () => {
    // Individually valid to the cache gate (under its 6h ceiling), but the
    // file is 90 minutes: served raw this is the whole-film skip button.
    localStorage.setItem(
      `kahawai.introdb.v1:tmdb:949:d5400000`,
      JSON.stringify({
        at: Date.now(),
        segments: [{ kind: 'intro', start_ms: 0, end_ms: 5_400_000, source: 'introdb' }],
      }),
    )
    const wire = answer({})
    vi.stubGlobal('fetch', wire)
    expect(await introdbSegments(film(), 5_400_000)).toEqual([])
    expect(wire).not.toHaveBeenCalled() // the entry is still an answer — just bounded
  })
})
