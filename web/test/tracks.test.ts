import { expect, test } from 'vitest'

import type { Preference } from '../src/api/generated/model/preference.ts'
import { sourcePreferenceScope } from '../src/domain/source.ts'
import { resolveTracks } from '../src/domain/tracks.ts'

const scope = sourcePreferenceScope([{ source_id: 7, collection_item_id: 'copy-b' }], 7)
const audio = [{ language: 'eng' }, { language: 'jpn' }, { language: 'eng' }]
const resolve = (prefs: Preference[]) =>
  resolveTracks(prefs, 'shared-work', 'movies', 'eng', audio, scope)

test('a legacy numeric work preference cannot choose an index on another copy', () => {
  expect(
    resolve([
      { scope: 'shared-work', key: 'audio', value: '#2' },
      { scope: '', key: 'audio.movies', value: 'eng' },
    ]).audioTrack,
  ).toBe(0)
})

test('a numeric subtitle memory cannot suppress the shared language wishlist', () => {
  expect(
    resolve([
      { scope: 'shared-work', key: 'subs', value: '#2' },
      { scope: 'shared-work', key: 'subs.track', value: '42' },
      { scope: '', key: 'subs.movies', value: 'eng' },
    ]),
  ).toMatchObject({ subs: ['eng'], subTrack: null })
})

test('qualified exact audio and subtitle choices still outrank portable language choices', () => {
  expect(
    resolve([
      { scope: scope!, key: 'audio.track', value: '#2' },
      { scope: scope!, key: 'subs.track', value: '42' },
      { scope: 'shared-work', key: 'audio', value: 'jpn' },
      { scope: 'shared-work', key: 'subs', value: 'fra' },
    ]),
  ).toEqual({ audioTrack: 2, subs: ['fra'], subTrack: 42 })
})

test('shared language choices remain portable across copies', () => {
  expect(
    resolve([
      { scope: 'shared-work', key: 'audio', value: 'jpn' },
      { scope: 'shared-work', key: 'subs', value: 'off' },
    ]),
  ).toEqual({ audioTrack: 1, subs: [], subTrack: null })
})
