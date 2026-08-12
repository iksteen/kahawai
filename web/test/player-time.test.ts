import assert from 'node:assert/strict'
import test from 'node:test'
import {
  absoluteMs,
  nextPartSeekMs,
  nudgeTarget,
  planSeek,
  producedEndMs,
} from '../src/player-time.ts'

test('position is the run offset plus the element clock', () => {
  assert.equal(
    absoluteMs({ known: true, offsetMs: 600_000, currentTimeS: 12, resumeMs: 0 }),
    612_000,
  )
})

test('before metadata, position is where we asked to resume', () => {
  // The element reads 0 there, which would put a viewer at the start of the
  // film — and that value reaches the progress report.
  assert.equal(
    absoluteMs({ known: false, offsetMs: 600_000, currentTimeS: 0, resumeMs: 599_000 }),
    599_000,
  )
})

test('produced end is 0 only when there is no seekable range', () => {
  assert.equal(producedEndMs({ offsetMs: 60_000, seekableEndS: 30 }), 90_000)
  assert.equal(producedEndMs({ offsetMs: 60_000, seekableEndS: null }), 0)
})

test('a seek inside the produced range is the element doing it', () => {
  const p = planSeek({ targetMs: 70_000, offsetMs: 60_000, producedEndS: 30, isHls: true })
  assert.deepEqual(p, { kind: 'in-run', toS: 10 })
})

test('a seek past the produced edge restarts the pipeline', () => {
  const p = planSeek({ targetMs: 200_000, offsetMs: 60_000, producedEndS: 30, isHls: true })
  assert.deepEqual(p, { kind: 'restart', toMs: 200_000 })
})

test('a seek BEFORE the run also restarts, rather than seeking negative', () => {
  // The case a naive `<= producedEnd` check gets wrong: -10s into the element
  // is not a position, and the run has to be rebuilt earlier.
  const p = planSeek({ targetMs: 50_000, offsetMs: 60_000, producedEndS: 30, isHls: true })
  assert.deepEqual(p, { kind: 'restart', toMs: 50_000 })
})

test('a direct file has the whole thing and never restarts', () => {
  const p = planSeek({ targetMs: 200_000, offsetMs: 0, producedEndS: 0, isHls: false })
  assert.deepEqual(p, { kind: 'whole-file', toS: 200 })
})

test('a nudge clamps to the duration, but only when there is one', () => {
  assert.equal(nudgeTarget({ posMs: 100_000, bySec: 30, durationMs: 3_000_000 }), 130_000)
  assert.equal(nudgeTarget({ posMs: 2_990_000, bySec: 30, durationMs: 3_000_000 }), 3_000_000)
  assert.equal(nudgeTarget({ posMs: 5_000, bySec: -10, durationMs: 3_000_000 }), 0)
  // No probed duration: clamping to 0 sent every nudge to the start of the
  // film, which is what this case exists to prevent.
  assert.equal(nudgeTarget({ posMs: 100_000, bySec: 30, durationMs: 0 }), 130_000)
})

test('a part ending mid-film moves into the next part', () => {
  assert.equal(
    nextPartSeekMs({ absMs: 1_500_000, durationMs: 3_000_000, parts: 2, isHls: true }),
    1_500_250,
  )
})

test('the last part ending is the film ending', () => {
  assert.equal(
    nextPartSeekMs({ absMs: 2_999_000, durationMs: 3_000_000, parts: 2, isHls: true }),
    null,
  )
})

test('single-part, direct, or unprobed sources never chase a next part', () => {
  const base = { absMs: 100_000, durationMs: 3_000_000, parts: 2, isHls: true }
  assert.equal(nextPartSeekMs({ ...base, parts: 1 }), null)
  assert.equal(nextPartSeekMs({ ...base, isHls: false }), null)
  assert.equal(nextPartSeekMs({ ...base, durationMs: 0 }), null)
})

test('both edges of the produced range belong to the element', () => {
  // The run origin itself and the produced edge are ordinary input: the edge
  // is where the seekbar is drawn to, so dragging to the end of what exists
  // is a normal drag. Restarting the pipeline for either rebuilds a stream
  // that already has the frame.
  assert.deepEqual(planSeek({ targetMs: 5000, offsetMs: 5000, producedEndS: 30, isHls: true }), {
    kind: 'in-run',
    toS: 0,
  })
  assert.deepEqual(planSeek({ targetMs: 35_000, offsetMs: 5000, producedEndS: 30, isHls: true }), {
    kind: 'in-run',
    toS: 30,
  })
  // A millisecond past it is not.
  assert.equal(
    planSeek({ targetMs: 35_001, offsetMs: 5000, producedEndS: 30, isHls: true }).kind,
    'restart',
  )
})
