vi.mock('../src/api/catalogue.ts', () => ({ catalogueDetail: vi.fn(), catalogueChildren: vi.fn() }))
import { beforeEach, describe, expect, test, vi } from 'vitest'
import type { ItemDetail } from '../src/api/catalogue-model.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  getPrefs: vi.fn(),
  startSession: vi.fn(),
  seekSession: vi.fn(),
  getSessionFileUrl: vi.fn(),
}))
vi.mock('../src/api/capabilities.ts', () => ({ buildProfile: vi.fn() }))

const api = await import('./api-fixture.ts')
const { buildProfile } = await import('../src/api/capabilities.ts')
const { queryPlaybackItem, selectPlaybackSource, startPlaybackSession } =
  await import('../src/api/playback.ts')
const profile = { containers: ['mp4'], video: [{ codec: 'h264' }] } as never
const detail = (selected = 1, secondLanguage = 'jpn') =>
  ({
    id: 'film',
    library_id: 'films',
    kind: 'movie',
    parent_id: null,
    metadata: null,
    negotiated: { source: { source_id: selected } },
    sources: [
      {
        source_id: 1,
        collection_item_id: 'copy-a',
        streams: { video: [], audio: [{ language: 'eng' }, { language: 'jpn' }] },
      },
      {
        source_id: 2,
        collection_item_id: 'copy-b',
        streams: { video: [], audio: [{ language: secondLanguage }, { language: 'eng' }] },
      },
    ],
  }) as unknown as ItemDetail

beforeEach(() => {
  vi.resetAllMocks()
  vi.mocked(buildProfile).mockReturnValue(profile)
  vi.mocked(api.startSession).mockResolvedValue({ session_id: 'session', source_id: 1 } as never)
})

describe('final playback selection', () => {
  test('reuses a preview only when its profile and every resolved index already agree', async () => {
    const selected = await selectPlaybackSource(detail(), [], 'movies', profile)
    expect(api.catalogueDetail).not.toHaveBeenCalled()
    expect(selected.sourceId).toBe(1)
    expect(selected.audioTrack).toBe(0)
  })

  test('ranks on the final source-aware profile even when every audio index is zero', async () => {
    vi.mocked(api.catalogueDetail).mockResolvedValue(detail(2))
    const selected = await selectPlaybackSource(detail(), [], 'movies', { containers: [] } as never)
    expect(api.catalogueDetail).toHaveBeenCalledWith(expect.any(String), 'film', {
      profile,
      source_audio_tracks: { 1: 0, 2: 0 },
    })
    await startPlaybackSession(selected.item, {
      prefs: [],
      sourceId: selected.sourceId,
      audioTrack: selected.audioTrack,
      profile: selected.profile,
    })
    expect(api.startSession).toHaveBeenCalledWith(
      expect.objectContaining({ profile, source_id: 2, audio_track: 0 }),
    )
  })

  test('a source changing its track order while QUERY runs is ranked again before START', async () => {
    const changed = detail(2, 'eng')
    changed.sources[1]!.streams!.audio[1]!.language = 'jpn'
    vi.mocked(api.catalogueDetail).mockResolvedValue(changed)
    const selected = await selectPlaybackSource(
      detail(),
      [{ scope: '', key: 'audio.movies', value: 'jpn' }],
      'movies',
      profile,
    )
    expect(api.catalogueDetail).toHaveBeenCalledTimes(2)
    expect(api.catalogueDetail).toHaveBeenLastCalledWith(expect.any(String), 'film', {
      profile,
      source_audio_tracks: { 1: 1, 2: 1 },
    })
    expect(selected.sourceId).toBe(2)
    expect(selected.audioTrack).toBe(1)
    expect(api.startSession).not.toHaveBeenCalled()
  })
})

test('start pins the stable catalogue rendition and maps its response to the displayed group', async () => {
  const item = detail(2)
  item.library_id = 'films'
  item.sources[1]!.media_entry_id = 'rendition-b'
  vi.mocked(api.startSession).mockResolvedValue({
    session_id: 'session',
    source_id: 9,
    media_entry_id: 'rendition-b',
  } as never)
  const result = await startPlaybackSession(item, { prefs: [], sourceId: 2 })
  expect(api.startSession).toHaveBeenCalledWith(
    expect.objectContaining({
      library_id: 'films',
      item_id: 'film',
      media_entry_id: 'rendition-b',
    }),
  )
  expect(result.source_id).toBe(2)
})

test('detail first GETs source facts then QUERYs once with resolved capabilities and audio', async () => {
  const facts = { ...detail(), negotiated: null }
  vi.mocked(api.catalogueDetail).mockResolvedValueOnce(facts).mockResolvedValueOnce(detail())
  await queryPlaybackItem('film', [], 'movies', undefined, 'films')
  expect(api.catalogueDetail).toHaveBeenCalledTimes(2)
  expect(api.catalogueDetail).toHaveBeenNthCalledWith(1, 'films', 'film')
  expect(api.catalogueDetail).toHaveBeenNthCalledWith(2, 'films', 'film', {
    profile,
    source_audio_tracks: { 1: 0, 2: 0 },
  })
})
