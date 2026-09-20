/// The Background work section's arithmetic: grouping, naming, and what a
/// count says.

import { describe, expect, test } from 'vitest'

import type { WorkQueue } from '../src/api/generated/model/workQueue.ts'
import {
  byArea,
  dueIn,
  needsAttention,
  progress,
  queueLabel,
  remaining,
} from '../src/domain/work.ts'

const queue = (over: Partial<WorkQueue> = {}): WorkQueue => ({
  area: 'subtitles',
  queue: 'text',
  host: null,
  collection: null,
  pending: 0,
  running: 0,
  retry: 0,
  blocked: 0,
  done: 0,
  next_due: null,
  error: null,
  rerun: true,
  ...over,
})

describe('grouping queues', () => {
  test('follows the area order and drops empty areas', () => {
    const groups = byArea([
      queue({ area: 'discovery', queue: 'loudness', host: 'nas', collection: 'films' }),
      queue({ area: 'enrichment', queue: 'tmdb' }),
    ])
    expect(groups.map((g) => g.area.id)).toEqual(['enrichment', 'discovery'])
  })

  test('keeps one mediahost’s rows together, then names them', () => {
    const groups = byArea([
      queue({ area: 'discovery', queue: 'loudness', host: 'nas', collection: 'shows' }),
      queue({ area: 'discovery', queue: 'scan', host: 'attic', collection: 'films' }),
      queue({ area: 'discovery', queue: 'cheap', host: 'nas', collection: 'films' }),
    ])
    expect(groups[0]!.queues.map((q) => `${q.host}/${q.collection}/${q.queue}`)).toEqual([
      'attic/films/scan',
      'nas/films/cheap',
      'nas/shows/loudness',
    ])
  })

  test('names a queue the way an operator would, and a stranger by its id', () => {
    expect(queueLabel('sets')).toBe('Display sets')
    expect(queueLabel('ocr')).toBe('Image OCR')
    expect(queueLabel('tmdb')).toBe('TMDB')
    expect(queueLabel('some-new-provider')).toBe('some-new-provider')
  })
})

describe('what a count says', () => {
  test('progress is done over everything, and discovery has no end to reach', () => {
    expect(progress(queue({ pending: 3, done: 1 }))).toEqual({ done: 1, total: 4, pct: 25 })
    expect(progress(queue({ area: 'discovery', pending: 3 }))).toEqual({
      done: 0,
      total: 3,
      pct: null,
    })
    expect(progress(queue()).pct).toBeNull()
  })

  test('remaining names only the non-zero states, or says idle', () => {
    expect(remaining(queue({ running: 1, blocked: 2 }))).toBe('1 running · 2 blocked')
    expect(remaining(queue({ done: 9 }))).toBe('idle')
  })

  test('a due time reads as a wait, and the past reads as now', () => {
    const now = 1_000_000
    expect(dueIn(queue({ pending: 1, next_due: now + 30 }), now)).toBe('in 30 s')
    expect(dueIn(queue({ retry: 1, next_due: now + 600 }), now)).toBe('in 10 min')
    expect(dueIn(queue({ retry: 1, next_due: now + 7200 }), now)).toBe('in 2 h')
    expect(dueIn(queue({ retry: 1, next_due: now - 5 }), now)).toBe('now')
    expect(dueIn(queue({ done: 4, next_due: now + 5 }), now)).toBeNull()
    expect(dueIn(queue({ pending: 1 }), now)).toBeNull()
  })

  test('attention is a blocked row or an error, not a busy queue', () => {
    expect(needsAttention(queue({ pending: 100 }))).toBe(false)
    expect(needsAttention(queue({ blocked: 1 }))).toBe(true)
    expect(needsAttention(queue({ error: 'tesseract failed' }))).toBe(true)
  })
})
