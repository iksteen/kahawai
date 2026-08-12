/// How an item names and measures itself in a list.
///
/// Untested until now, which mattered: `resumeMsFor` could be replaced with
/// `return 0` and the whole suite passed. Every item would resume from the
/// start, and nothing would say so.

import assert from 'node:assert/strict'
import test from 'node:test'
import type { Item } from '../src/api.ts'
import {
  episodeOf,
  metaLine,
  projecting,
  resumeMsFor,
  seasonLabel,
  seasonOf,
  seLabel,
  targetOf,
  watchedPct,
} from '../src/label.ts'

const item = (fields: Partial<Item>): Item => ({ id: 'i', kind: 'movie', ...fields }) as Item

test('an episode names its season and number, padded', () => {
  assert.equal(seLabel(1, 2), 'S01E02')
  assert.equal(seLabel(12, 34), 'S12E34')
})

test('absolute numbering prints no season rather than an unknown one', () => {
  // Anime is numbered this way on purpose; S?E11 reads as data we lost.
  assert.equal(seLabel(null, 11), 'E11')
})

test('specials are season zero, not a missing season', () => {
  assert.equal(seLabel(0, 1), 'S00E01')
  assert.equal(seasonLabel(0, false), 'Specials')
})

test('a batch file spanning episodes says so', () => {
  assert.equal(seLabel(1, 2, 3), 'S01E02-03')
})

test('a null season reads differently depending on whether a projection exists', () => {
  // With one, the unprojected leftovers are "Other"; without, absolute
  // numbering IS the numbering, and the page is just "Episodes".
  assert.equal(seasonLabel(null, true), 'Other')
  assert.equal(seasonLabel(null, false), 'Episodes')
  assert.equal(seasonLabel(2, true), 'Season 2')
})

test('projecting takes both the preference and something to project', () => {
  const withProj = [item({ proj_season: 1 })]
  assert.equal(projecting('seasons', withProj), true)
  assert.equal(projecting('native', withProj), false)
  assert.equal(projecting('seasons', [item({ season: 1 })]), false)
  assert.equal(projecting('seasons', []), false)
})

test('a projected episode is numbered by the projection, an unprojected one by itself', () => {
  const e = item({ season: null, episode: 11, proj_season: 1, proj_episode: 3 })
  assert.equal(seasonOf(e, true), 1)
  assert.equal(episodeOf(e, true), 3)
  assert.equal(seasonOf(e, false), null)
  assert.equal(episodeOf(e, false), 11)
})

test('an episode with no projected number keeps its own', () => {
  // The season does NOT fall back the same way: null there means absolute
  // numbering, which is an answer rather than a gap.
  const e = item({ season: 2, episode: 11, proj_season: null, proj_episode: null })
  assert.equal(episodeOf(e, true), 11)
  assert.equal(seasonOf(e, true), null)
})

test('resume keeps a position that is not yet the credits', () => {
  assert.equal(
    resumeMsFor(item({ resume_position_ms: 600_000, resume_duration_ms: 1_000_000 })),
    600_000,
  )
})

test('resume gives up exactly at nine tenths, not after it', () => {
  // The boundary in both directions: one below resumes, the boundary itself
  // and anything past it starts over, so a viewer who finished is not put
  // back into the last four minutes.
  assert.equal(
    resumeMsFor(item({ resume_position_ms: 899_999, resume_duration_ms: 1_000_000 })),
    899_999,
  )
  assert.equal(resumeMsFor(item({ resume_position_ms: 900_000, resume_duration_ms: 1_000_000 })), 0)
  assert.equal(resumeMsFor(item({ resume_position_ms: 990_000, resume_duration_ms: 1_000_000 })), 0)
})

test('an item with nothing stored resumes from the start', () => {
  assert.equal(resumeMsFor(item({})), 0)
  assert.equal(resumeMsFor(item({ resume_position_ms: 5_000 })), 0)
  assert.equal(resumeMsFor(item({ resume_duration_ms: 5_000 })), 0)
})

test('progress is a percentage of the whole item', () => {
  assert.equal(watchedPct(item({ resume_position_ms: 250, resume_duration_ms: 1000 })), 25)
})

test('progress cannot exceed the bar it is drawn in', () => {
  // A stored position past the duration is not impossible — a re-probe can
  // shorten an item — and an unclamped one draws a fill wider than its track.
  assert.equal(watchedPct(item({ resume_position_ms: 2000, resume_duration_ms: 1000 })), 100)
})

test('unstarted or unmeasured is no bar at all, which is not zero percent', () => {
  assert.equal(watchedPct(item({})), null)
  assert.equal(watchedPct(item({ resume_position_ms: 0, resume_duration_ms: 1000 })), null)
  assert.equal(watchedPct(item({ resume_position_ms: 100, resume_duration_ms: 0 })), null)
})

test('an episode says which show it came from', () => {
  const e = item({ kind: 'episode', parent_title: 'A Show', season: 1, episode: 2 })
  assert.equal(metaLine(e), 'A Show S01E02')
})

test('every other kind is told apart by what distinguishes it', () => {
  // A track by who played it and what it is on; an album by who made it and
  // when, because there are several Greatest Hits and several pressings of
  // one of them; everything else by its year alone.
  assert.equal(
    metaLine(item({ kind: 'track', artist: 'A Band', parent_title: 'An Album' })),
    'A Band · An Album',
  )
  assert.equal(metaLine(item({ kind: 'album', artist: 'A Band', year: 1994 })), 'A Band · 1994')
  assert.equal(metaLine(item({ kind: 'movie', year: 1994 })), '1994')
  assert.equal(metaLine(item({ kind: 'movie' })), '')
})

test('an episode with no number still numbers itself', () => {
  // `episode` is nullable and `metaLine` feeds it straight in, so the
  // fallback decides what a card says rather than being unreachable.
  assert.equal(seLabel(1, null), 'S01E00')
  assert.equal(seLabel(null, null), 'E00')
})

test('a track opens its album, because it has no page of its own', () => {
  assert.equal(targetOf(item({ id: 't', kind: 'track', parent_id: 'alb' })), 'alb')
  assert.equal(targetOf(item({ id: 't', kind: 'track' })), 't')
  assert.equal(targetOf(item({ id: 'm', kind: 'movie', parent_id: 'x' })), 'm')
})
