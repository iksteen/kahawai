/// The grid's arithmetic, on its own. `library.test.ts` drives the same
/// numbers through a mounted view with the measurements stubbed; this is where
/// the edges live — the roundings, the empty library, the last short row.

import { describe, expect, test } from 'vitest'

import {
  cellsIn,
  changed,
  CHUNK,
  chunksFor,
  countLine,
  GAP,
  type Metric,
  OVERSCAN,
  reservedHeight,
  shapeOf,
  visibleRows,
} from '../src/domain/virtual.ts'

/// Four columns, rows 200px apart.
const metric: Metric = { cols: 4, rowH: 200 }
const view = (scrollY: number) => ({ wrapTop: 100, scrollY, height: 800 })

describe('reserving the whole library', () => {
  test('is every row, less the gap under the last one', () => {
    // 10 items over 4 columns is 3 rows. Written out rather than derived from
    // GAP: re-deriving the answer from the same constant the code uses means
    // setting it to zero passes.
    expect(GAP).toBe(14)
    expect(reservedHeight(10, metric)).toBe(586)
  })

  test('an empty library still reserves one row', () => {
    // Zero height would collapse the wrapper and take the measurement with it.
    expect(reservedHeight(0, metric)).toBe(200 - GAP)
  })

  test('and a part-full last row counts as a row', () => {
    expect(reservedHeight(9, metric)).toBe(3 * 200 - GAP)
    expect(reservedHeight(8, metric)).toBe(2 * 200 - GAP)
  })
})

describe('which rows are live', () => {
  test('at the top, the overscan does not go negative', () => {
    expect(visibleRows(view(0), metric, 400).start).toBe(0)
  })

  test('scrolled down, it is the window plus the overscan either side', () => {
    // 2100 past the top of the grid is row 10; 800 tall is 4 rows deep. The
    // numbers are written out for the same reason as the gap above.
    expect(OVERSCAN).toBe(3)
    const rows = visibleRows(view(2100), metric, 400)
    expect(rows).toEqual({ start: 7, end: 17 })
  })

  test('a part-scrolled row is still on screen, and a part-covered one still counts', () => {
    // Every fixture being an exact multiple hid both roundings: the first
    // visible row rounds DOWN, because a row half off the top is half on, and
    // the depth rounds UP, because a window that ends mid-row is still over
    // that row.
    const rows = visibleRows({ wrapTop: 100, scrollY: 2150, height: 850 }, metric, 400)
    // 2050 into the grid is row 10.25 → row 10, less the overscan.
    expect(rows.start).toBe(7)
    // 850 tall is 4.25 rows → 5.
    expect(rows.end).toBe(10 + 5 + OVERSCAN)
  })

  test('and it never runs past the end of the library', () => {
    // 10 items over 4 columns is 3 rows, so the last row is 2.
    expect(visibleRows(view(0), metric, 10).end).toBe(2)
  })
})

describe('which cells those rows hold', () => {
  test('in order, left to right', () => {
    expect(cellsIn({ start: 0, end: 1 }, metric, 100)).toEqual([0, 1, 2, 3, 4, 5, 6, 7])
  })

  test('and the last row is usually short', () => {
    // Nothing past the end: a cell index the library does not have would
    // render a placeholder that never fills.
    expect(cellsIn({ start: 0, end: 1 }, metric, 6)).toEqual([0, 1, 2, 3, 4, 5])
  })
})

describe('which chunks they need', () => {
  test('the first rows are the first chunk', () => {
    expect(chunksFor({ start: 0, end: 5 }, metric, 1000)).toEqual([0])
  })

  test('a window that straddles a boundary needs both', () => {
    // Row 24 ends at item 99, row 25 starts at 100.
    expect(chunksFor({ start: 24, end: 25 }, metric, 1000)).toEqual([0, 1])
  })

  test('and it never asks for a chunk past the end', () => {
    // A library of 150 has chunks 0 and 1 and no more, however far the
    // overscan reaches.
    expect(chunksFor({ start: 30, end: 60 }, metric, 150)).toEqual([1])
  })

  test('an empty library needs none', () => {
    // `total - 1` is -1, and asking for chunk -1 is a request the hub answers
    // with an error nobody can act on.
    expect(chunksFor({ start: 0, end: 5 }, metric, 0)).toEqual([])
  })

  test('one chunk covers a hundred items', () => {
    expect(chunksFor({ start: 0, end: 100 / metric.cols - 1 }, metric, 1000)).toEqual([0])
    expect(CHUNK).toBe(100)
  })
})

describe('when to re-measure', () => {
  test('the first measurement always counts', () => {
    expect(changed(null, metric)).toBe(true)
  })

  test('a different column count counts', () => {
    expect(changed(metric, { cols: 3, rowH: 200 })).toBe(true)
  })

  test('and a hair of jitter does not', () => {
    // Measuring writes state, so a render-driven measurement is a cycle; a
    // fraction of a pixel of text-metric noise must not start it.
    expect(changed(metric, { cols: 4, rowH: 200.4 })).toBe(false)
    expect(changed(metric, { cols: 4, rowH: 201 })).toBe(true)
  })
})

describe('the shape of a card', () => {
  test('a sleeve is square, and sits in a wider column', () => {
    expect(shapeOf('music')).toEqual({ '--card-min': '150px', '--card-ratio': '1' })
  })

  test('and everything else is a poster', () => {
    expect(shapeOf('movies')['--card-ratio']).toBe('2 / 3')
    expect(shapeOf('')['--card-ratio']).toBe('2 / 3')
  })
})

describe('the count line', () => {
  test('says nothing until the first answer', () => {
    expect(countLine(null, null, false)).toBe('')
  })

  test('is the library’s size when nothing is filtered', () => {
    expect(countLine(2242, 2242, false)).toBe('2242')
  })

  test('and says what it is filtering when something is', () => {
    // 12 on its own leaves you wondering whether the other 2230 are missing or
    // excluded.
    expect(countLine(12, 2242, true)).toBe('12/2242')
  })

  test('but not before it knows what it is filtering from', () => {
    expect(countLine(12, null, true)).toBe('12')
  })
})
