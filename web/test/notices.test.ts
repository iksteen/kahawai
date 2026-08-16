import { afterEach, beforeEach, expect, test, vi } from 'vitest'

import { NOTICE_MS, clearNotices, notice, notify } from '../src/composables/notices.ts'

beforeEach(() => vi.useFakeTimers())
afterEach(() => {
  clearNotices()
  vi.useRealTimers()
})

test('a notice shows and then goes', () => {
  notify('Could not mark that watched.')
  expect(notice.value).toBe('Could not mark that watched.')
  vi.advanceTimersByTime(NOTICE_MS)
  expect(notice.value).toBe('')
})

test('a second notice replaces the first and restarts its clock', () => {
  // Not a queue: two failures in a row are one thing to say, and the newer
  // one is the true one. A queue would show a stale sentence after the
  // situation it described had changed.
  notify('first')
  vi.advanceTimersByTime(NOTICE_MS - 100)
  notify('second')
  expect(notice.value).toBe('second')
  vi.advanceTimersByTime(NOTICE_MS - 100)
  expect(notice.value).toBe('second')
  vi.advanceTimersByTime(100)
  expect(notice.value).toBe('')
})
