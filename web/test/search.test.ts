/// The search box's behaviour, which is mostly about what does NOT happen:
/// the query not changing twice per keystroke, the panel not following you to
/// a screen nobody searched, and a dismissed panel not becoming unreachable.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { nextTick, type Ref, watch } from 'vue'

import { DEBOUNCE_MS, useSearch } from '../src/composables/search.ts'

beforeEach(() => vi.useFakeTimers())
afterEach(() => vi.useRealTimers())

async function type(search: ReturnType<typeof useSearch>, value: string, panel = true) {
  search.typed(value, panel)
  await nextTick()
}

/// Every value the query takes, in order.
///
/// `flush: 'sync'` is the whole point. A default watcher is `flush: 'pre'`, so
/// two writes inside one `advanceTimersByTime` coalesce into a single callback
/// carrying the last value — and the intermediate value this file exists to
/// forbid becomes unobservable. Measured: with a `pre` watcher, deleting the
/// `clearTimeout` from the composable's debounce broke nothing in this suite.
function record(query: Readonly<Ref<string>>) {
  const seen: string[] = []
  watch(query, (value) => seen.push(value), { flush: 'sync' })
  return seen
}

describe('debouncing', () => {
  test('the query settles once, not once per keystroke', async () => {
    const s = useSearch()
    const seen = record(s.query)
    await type(s, 'h')
    await type(s, 'he')
    await type(s, 'hea')
    expect(s.query.value).toBe('')
    vi.advanceTimersByTime(DEBOUNCE_MS)
    // Once. Three surviving timers would fire in order and still leave 'hea'
    // as the final value, so the final value alone proves nothing.
    expect(seen).toEqual(['hea'])
  })

  test('it is still waiting one tick before the interval', async () => {
    // Advancing by exactly DEBOUNCE_MS compares the constant against itself:
    // setting it to 0 passed every test in this file, because a 0 ms timer
    // still does not run until the clock is advanced.
    const s = useSearch()
    await type(s, 'hea')
    vi.advanceTimersByTime(DEBOUNCE_MS - 1)
    expect(s.query.value).toBe('')
    vi.advanceTimersByTime(1)
    expect(s.query.value).toBe('hea')
  })

  test('the box itself is immediate', async () => {
    // The caret cannot wait for a debounce.
    const s = useSearch()
    await type(s, 'hea')
    expect(s.text.value).toBe('hea')
  })
})

describe('when the panel shows', () => {
  test('typing opens it, emptying the box closes it', async () => {
    const s = useSearch()
    await type(s, 'heat')
    expect(s.open.value).toBe(true)
    await type(s, '')
    expect(s.open.value).toBe(false)
  })

  test('a screen with no panel never opens one', async () => {
    // The flag outlived the route once: typing in a library's filter set it
    // with nothing to show, and going home mounted the panel already open
    // over a page nobody had searched.
    const s = useSearch()
    await type(s, 'heat', false)
    expect(s.open.value).toBe(false)
  })

  test("and focusing that screen's box does not open one either", () => {
    // The same guard on the other entry point. `typed` had a test and `reopen`
    // did not, so dropping the check from `reopen` alone passed the suite.
    const s = useSearch()
    s.typed('heat', true)
    s.dismiss()
    s.reopen(false)
    expect(s.open.value).toBe(false)
  })

  test('whitespace is not a search', async () => {
    // A space would otherwise open the panel and fan out one request per
    // library for nothing.
    const s = useSearch()
    await type(s, '   ')
    expect(s.open.value).toBe(false)
    s.reopen(true)
    expect(s.open.value).toBe(false)
  })

  test('dismissing keeps the text, and coming back reopens', async () => {
    // Dismissing must not clear the box, and the panel must not become
    // unreachable: a click opened a library and left focus in the box, so no
    // focus event could fire again and editing the text was the only way back
    // to results already fetched.
    const s = useSearch()
    await type(s, 'heat')
    s.dismiss()
    expect(s.open.value).toBe(false)
    expect(s.text.value).toBe('heat')
    s.reopen(true)
    expect(s.open.value).toBe(true)
  })

  test('reopening an empty box shows nothing', async () => {
    const s = useSearch()
    s.reopen(true)
    expect(s.open.value).toBe(false)
  })
})

describe('leaving', () => {
  test('going home clears the standing filter', async () => {
    // A filter that silently follows you home reads as missing items.
    const s = useSearch()
    await type(s, 'heat')
    vi.advanceTimersByTime(DEBOUNCE_MS)
    s.clear()
    expect(s.text.value).toBe('')
    expect(s.query.value).toBe('')
    expect(s.open.value).toBe(false)
  })

  test('opening a result takes the text with it', () => {
    // You asked for this one thing and got it; the box that found it is not a
    // filter you meant to leave standing on the item page.
    const s = useSearch()
    s.typed('heat', true)
    s.taken()
    expect(s.text.value).toBe('')
    expect(s.open.value).toBe(false)
  })

  test('an abandoned keystroke never reaches the query at all', async () => {
    // Asserting the FINAL value would pass either way: clearing sets the text
    // to empty, which schedules its own debounce that blanks the query a
    // moment later. What matters is that the abandoned word is never SEEN —
    // one tick of it is a request against the screen you have just left.
    //
    // What provides that is the watcher clearing the pending timer, not
    // anything in `clear` — Vue flushes watchers on the microtask queue, so
    // they always run before a timer can. Worth a test precisely because the
    // guarantee lives somewhere other than where you would look for it.
    const s = useSearch()
    const seen = record(s.query)

    await type(s, 'heat')
    s.clear()
    await nextTick()
    vi.advanceTimersByTime(DEBOUNCE_MS * 4)
    await nextTick()

    expect(seen).not.toContain('heat')
  })
})
