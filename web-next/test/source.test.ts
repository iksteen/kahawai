/// How a file describes itself. Every case here is one the old page hit: 26
/// chips saying "text", a plan row whose action was coloured by parsing a
/// sentence, and a subtitle verdict that answered a different question from
/// the one the row exists for.

import { describe, expect, test } from 'vitest'

import {
  deliveryPlan,
  duration,
  groupSources,
  LOUD_DELIVERY,
  planRow,
  size,
  subtitleChip,
  subtitleChipTitle,
  subtitleVerdict,
} from '../src/domain/source.ts'

describe('a running time', () => {
  test('is minutes, and hours once there are enough of them', () => {
    expect(duration(90 * 60_000)).toBe('1 h 30 min')
    expect(duration(45 * 60_000)).toBe('45 min')
    expect(duration(60 * 60_000)).toBe('1 h 0 min')
  })

  test('and nothing at all when nobody knows it', () => {
    // A browse row has no duration; printing "0 min" states a fact about the
    // file that nothing has measured.
    expect(duration(undefined)).toBeNull()
    expect(duration(null)).toBeNull()
    expect(duration(0)).toBeNull()
  })
})

describe('a file size', () => {
  test('is gigabytes, to one place', () => {
    expect(size(2.5 * 1024 ** 3)).toBe('2.5 GB')
  })
})

describe('the subtitle chip', () => {
  test('is one chip, however many tracks there are', () => {
    // 26 embedded tracks produced 26 chips all reading "text", which said
    // nothing 26 times and pushed the size and the offline mark off the row.
    const many = Array.from({ length: 26 }, () => ({ format: 'text', language: null }))
    expect(subtitleChip(many)).toBe('26 subs · text')
  })

  test('names the languages while there are few enough to read', () => {
    expect(subtitleChip([{ format: 'srt', language: 'en' }])).toBe('1 sub · en')
    expect(
      subtitleChip([
        { format: 'srt', language: 'en' },
        { format: 'srt', language: 'nl' },
      ]),
    ).toBe('2 subs · en nl')
  })

  test('and falls back to formats past a handful', () => {
    const seven = ['en', 'nl', 'de', 'fr', 'es', 'it', 'pt'].map((language) => ({
      format: 'srt',
      language,
    }))
    expect(subtitleChip(seven)).toBe('7 subs · srt')
  })

  test('the same language twice is one language', () => {
    expect(
      subtitleChip([
        { format: 'srt', language: 'en' },
        { format: 'ass', language: 'en' },
      ]),
    ).toBe('2 subs · en')
  })

  test('and the full list goes in the tooltip, where length costs nothing', () => {
    expect(
      subtitleChipTitle([
        { format: 'srt', language: 'en' },
        { format: 'ass', language: null },
      ]),
    ).toBe('en srt, ass')
  })
})

describe('a plan row', () => {
  test('splits the action from the reasoning', () => {
    const row = planRow('dts → aac (transcoded) — 7.1 → 5.1')
    expect(row.action).toBe('dts → aac (transcoded)')
    expect(row.why).toBe('7.1 → 5.1')
  })

  test('keeps every dash after the first one in the reasoning', () => {
    expect(planRow('a — b — c').why).toBe('b — c')
  })

  test('the cheap outcomes read as cheap', () => {
    expect(planRow('copy').tone).toBe('teal')
    expect(planRow('direct').tone).toBe('teal')
    expect(planRow('text').tone).toBe('teal')
  })

  test('and the words are matched however they are cased', () => {
    // The hub writes them; the client must not depend on which case it used.
    expect(planRow('COPY').tone).toBe('teal')
    expect(planRow('Direct — from disk').tone).toBe('teal')
  })

  test('and anything that costs something does not', () => {
    expect(planRow('h264 → h265 (encoded)').tone).toBe('sand')
  })

  test('a tone the caller supplies wins', () => {
    // The subtitle row's tone comes from its delivery, which the wording of
    // the action cannot be read off.
    expect(planRow('copy', 'warn').tone).toBe('warn')
  })
})

describe('the subtitle verdict', () => {
  test('says which track you get, and how', () => {
    expect(subtitleVerdict({ language: 'en', format: 'srt', delivery: 'text' }, 2).verdict).toBe(
      'en srt · text',
    )
  })

  test('and does not say it twice when the delivery IS the format', () => {
    // An ASS track delivered as ASS does not need saying twice; a text track
    // delivered as a burn very much does.
    expect(subtitleVerdict({ language: 'en', format: 'ass', delivery: 'ass' }, 2).verdict).toBe(
      'en ass',
    )
  })

  test('a delivery that costs something is coloured', () => {
    expect(subtitleVerdict({ language: 'en', format: 'srt', delivery: 'burn' }, 2).tone).toBe(
      LOUD_DELIVERY.burn,
    )
    expect(
      subtitleVerdict({ language: 'en', format: 'srt', delivery: 'text' }, 2).tone,
    ).toBeUndefined()
  })

  test('nothing matching is not the same as wanting nothing', () => {
    // "off" is a preference; "none" is a failure to satisfy one, and only the
    // second is worth a colour.
    expect(subtitleVerdict(undefined, 0)).toEqual({ verdict: 'off' })
    expect(subtitleVerdict(undefined, 2)).toEqual({ verdict: 'none', tone: 'sand' })
  })

  test('and a track with no language still names its format', () => {
    expect(subtitleVerdict({ language: null, format: 'pgs', delivery: 'overlay' }, 1).verdict).toBe(
      '? pgs · overlay',
    )
  })
})

describe('the plan chip', () => {
  test('names the work, not the pipeline', () => {
    // `mode` says whether bytes are served as they are; `cost` says whether
    // anything is re-encoded, and a remux carrying `video_encode` does.
    expect(deliveryPlan('direct').chip).toBe('DIRECT')
    expect(deliveryPlan('copy').chip).toBe('REMUX')
    expect(deliveryPlan('audio_encode').chip).toBe('TRANSCODE')
    expect(deliveryPlan('video_encode').chip).toBe('TRANSCODE')
  })

  test('and the cheap ones read as cheap', () => {
    expect(deliveryPlan('direct').tone).toBe('teal')
    expect(deliveryPlan('copy').tone).toBe('teal')
    expect(deliveryPlan('video_encode').tone).toBe('sand')
    expect(deliveryPlan('unplayable').tone).toBe('warn')
  })

  test('only the two that a stream row cannot explain carry a note', () => {
    // The panel lists every stream's verdict underneath, so a sentence beside
    // the chip said the same thing twice — and worse, because one chip cannot
    // describe two streams.
    expect(deliveryPlan('direct').note).toBe('')
    expect(deliveryPlan('copy').note).toBe('')
    expect(deliveryPlan('video_encode').note).toBe('')
    expect(deliveryPlan('audio_encode').note).toContain('picture is copied')
    // Nothing to read off a row: there is no work, there is a refusal.
    expect(deliveryPlan('unplayable').note).toContain('nothing here can be delivered')
  })

  test('and a cost this build has never heard of is still shown', () => {
    // The hub may know something this client does not, and hiding it would
    // leave a panel that says nothing about what is about to happen.
    expect(deliveryPlan('something_new')).toEqual({
      chip: 'SOMETHING_NEW',
      tone: 'warn',
      note: '',
    })
  })
})

describe('grouping the files an item is made of', () => {
  const file = (source_id: number, part: number, parts: number) => ({ source_id, part, parts })

  test('parts of one work are one entry, in order', () => {
    // UI-27: a film in seven numbered parts and seven alternative encodes were
    // both "7 sources", in an order that means nothing to a reader.
    const grouped = groupSources([file(1, 2, 3), file(1, 1, 3), file(1, 3, 3)])
    expect(grouped).toHaveLength(1)
    expect(grouped[0]!.parts.map((p) => p.part)).toEqual([1, 2, 3])
  })

  test('and alternatives are separate entries', () => {
    expect(groupSources([file(1, 1, 1), file(2, 1, 1)])).toHaveLength(2)
  })

  test('a work missing a part says so', () => {
    // It cannot play to the end, and "7 sources" over it hides that.
    expect(groupSources([file(1, 1, 3), file(1, 2, 3)])[0]!.whole).toBe(false)
    expect(groupSources([file(1, 1, 2), file(1, 2, 2)])[0]!.whole).toBe(true)
  })

  test('and the order the hub sent them in is kept between works', () => {
    // The list is ordered by what playback would pick, so the first entry is
    // the one Play would use.
    expect(groupSources([file(7, 1, 1), file(3, 1, 1)]).map((g) => g.id)).toEqual([7, 3])
  })
})
