/// The search overlay's rows, and moving through them with the arrow keys.
///
/// The panel itself cannot be tested here — there is no DOM renderer in this
/// suite — so the arithmetic lives in `search-nav.ts` and this is what covers
/// it. Everything below is a case that behaves differently under a plausible
/// wrong implementation: wrapping instead of clamping, skipping headings,
/// counting the limit rather than the rows returned.

import assert from 'node:assert/strict'
import test from 'node:test'
import { countLabel, moveHighlight, searchRows, type LibraryHits } from '../src/search-nav.ts'
import type { Item, LibrarySummary } from '../src/api.ts'

const lib = (id: string, name: string): LibrarySummary => ({ id, name, media_type: 'movies' })
const item = (id: string): Item => ({ id, kind: 'movie', title: id }) as Item

const films = lib('L1', 'Films')
const shows = lib('L2', 'Series')

test('a library heading is followed by its own items, in library order', () => {
  const hits: LibraryHits[] = [
    { library: films, items: [item('a'), item('b')], total: 2 },
    { library: shows, items: [item('c')], total: 1 },
  ]
  // Reading `library` on the ITEM rows too, not just the headings: that field
  // is what navigation uses, and a version attributing every hit to the first
  // library's name passes any assertion that only looks at item ids.
  assert.deepEqual(
    searchRows(hits).map((r) =>
      r.kind === 'library' ? `[${r.library.name}]` : `${r.library.id}/${r.item.id}`,
    ),
    ['[Films]', 'L1/a', 'L1/b', '[Series]', 'L2/c'],
  )
})

test('a library with nothing in it contributes no heading', () => {
  // A heading over an empty group reads as "we looked and there is something
  // here". The search already filters these out, but the row builder must not
  // depend on that.
  const hits: LibraryHits[] = [
    { library: films, items: [], total: 0 },
    { library: shows, items: [item('c')], total: 1 },
  ]
  assert.deepEqual(
    searchRows(hits).map((r) => (r.kind === 'library' ? `[${r.library.name}]` : r.item.id)),
    ['[Series]', 'c'],
  )
})

test('the heading says how much of the match it is showing', () => {
  // `shown` is the rows actually returned, not the limit asked for: a library
  // with three matches must not claim to be showing five of three.
  assert.equal(countLabel(5, 12), '5 of 12')
  assert.equal(countLabel(3, 3), '3')
  // Showing everything there is, whatever the number happens to be.
  assert.equal(countLabel(5, 5), '5')
  assert.equal(countLabel(5, 6), '5 of 6')
  // The case that pins the contract rather than today's caller: the number
  // shown is whatever came back, not whatever was asked for. Reading the limit
  // instead — `total > 5 ? '5 of N'` — agrees with every assertion above and
  // lies here, claiming five rows over three.
  assert.equal(countLabel(3, 10), '3 of 10')
})

test('down from nothing highlighted lands on the first row', () => {
  assert.equal(moveHighlight(5, -1, 1), 0)
})

test('up from nothing highlighted stays at nothing', () => {
  // Pressing up before pressing down must not jump to the bottom of a list you
  // have not looked at.
  assert.equal(moveHighlight(5, -1, -1), -1)
})

test('the ends clamp rather than wrap', () => {
  // Wrapping in a panel you cannot see all of loses your place silently.
  assert.equal(moveHighlight(3, 2, 1), 2)
  assert.equal(moveHighlight(3, 0, -1), 0)
})

test('a highlight left beyond the end is brought back into range', () => {
  // The reachable case, and the one that tells clamping apart from "at the end,
  // stay put": a result set can shrink under a highlight — three rows with the
  // highlight at seven — and returning `from` would leave it off the end for
  // good, with no row lit and Enter opening nothing.
  assert.equal(moveHighlight(3, 7, -1), 2)
  assert.equal(moveHighlight(3, 7, 1), 2)
})

test('a heading is a stop, not something to skip', () => {
  // The executable statement of the design rather than a check on
  // `moveHighlight`, which takes a count and so cannot see kinds at all: no
  // implementation of it could skip a heading. This breaks if `searchRows` ever
  // stops putting headings in the same list, which is the thing worth pinning.
  const rows = searchRows([
    { library: films, items: [item('a'), item('b')], total: 2 },
    { library: shows, items: [item('c')], total: 1 },
  ])
  const at = moveHighlight(rows.length, 2, 1)
  assert.equal(at, 3)
  assert.equal(rows[at].kind, 'library')
})

test('an empty result set has nothing to highlight', () => {
  assert.equal(moveHighlight(0, -1, 1), -1)
  assert.equal(moveHighlight(0, 3, -1), -1)
})
