/// A grid whose whole height is reserved from the first response.
///
/// That is the difference from infinite scroll, where the page grows as you go
/// and the scrollbar jumps under the thumb every time it does. Only the rows on
/// screen exist in the DOM; the scrollbar describes all of them.
///
/// The arithmetic lives here because it is the part that can be checked: a
/// test environment has no layout, so the numbers the component measures off
/// the page have to come from somewhere a test can supply them.

/// Items per request. Smaller than the hub's 200 default because a chunk is
/// fetched to fill a viewport, not to be a page someone reads.
export const CHUNK = 100

/// Rows kept live above and below the viewport, so a flick of the wheel lands
/// on cards that are already there.
export const OVERSCAN = 3

/// Must match the grid's `gap` — the one number the component cannot measure,
/// because it is between the cells rather than in one.
export const GAP = 14

/// How a library's items are shaped. A sleeve is square and a poster is two by
/// three, and the narrower shape can sit in a narrower column.
export function shapeOf(mediaType: string): Record<string, string> {
  return mediaType === 'music'
    ? { '--card-min': '150px', '--card-ratio': '1' }
    : { '--card-min': '140px', '--card-ratio': '2 / 3' }
}

/// What the page has measured: how many columns CSS resolved to, and the pitch
/// of a row — the cell's height plus the gap.
export type Metric = { cols: number; rowH: number }

export type Viewport = {
  /// Where the grid starts, in document coordinates.
  wrapTop: number
  scrollY: number
  height: number
}

/// How tall the whole library is. The last row has no gap under it.
export function reservedHeight(total: number, metric: Metric): number {
  return Math.max(1, Math.ceil(total / metric.cols)) * metric.rowH - GAP
}

/// Which rows the window is over, with the overscan either side.
export function visibleRows(
  view: Viewport,
  metric: Metric,
  total: number,
): { start: number; end: number } {
  const firstVisible = Math.floor((view.scrollY - view.wrapTop) / metric.rowH)
  const deep = Math.ceil(view.height / metric.rowH)
  const totalRows = Math.ceil(total / metric.cols)
  return {
    start: Math.max(0, firstVisible - OVERSCAN),
    end: Math.min(totalRows - 1, firstVisible + deep + OVERSCAN),
  }
}

/// The item indices those rows hold, in order. Nothing past the end of the
/// library: the last row is usually short.
export function cellsIn(
  rows: { start: number; end: number },
  metric: Metric,
  total: number,
): number[] {
  const cells: number[] = []
  for (let row = rows.start; row <= rows.end; row++) {
    for (let col = 0; col < metric.cols; col++) {
      const at = row * metric.cols + col
      if (at < total) cells.push(at)
    }
  }
  return cells
}

/// Which chunks those rows need. By chunk NUMBER rather than by offset, so a
/// caller can hold "asked" and "failed" as sets of the same thing.
export function chunksFor(
  rows: { start: number; end: number },
  metric: Metric,
  total: number,
): number[] {
  const from = rows.start * metric.cols
  // An empty library needs no guard of its own: `to` is -1, `Math.floor(-1 /
  // CHUNK)` is -1, and the loop below never runs. One was written and no test
  // could tell it apart.
  const to = Math.min(total - 1, rows.end * metric.cols + metric.cols - 1)
  const chunks: number[] = []
  for (let c = Math.floor(from / CHUNK); c <= Math.floor(to / CHUNK); c++) chunks.push(c)
  return chunks
}

/// Whether a fresh measurement is worth taking.
///
/// The half-pixel tolerance exists so a hair of text-metric jitter cannot
/// start a measure/render loop. Its cost is that the same hair is multiplied
/// by the row count — measured at 5px of slack over 321 rows of a 2242-album
/// library, 0.007% of the reserved height.
export function changed(was: Metric | null, now: Metric): boolean {
  if (!was) return true
  return was.cols !== now.cols || Math.abs(was.rowH - now.rowH) > 0.5
}

/// What the count line says.
///
/// Filtering says what it is filtering: 12 on its own leaves you wondering
/// whether the other 2230 are missing or excluded.
export function countLine(total: number | null, libraryTotal: number | null, filtered: boolean) {
  if (total === null) return ''
  return filtered && libraryTotal !== null ? `${total}/${libraryTotal}` : String(total)
}
