import { beforeEach, describe, expect, test, vi } from 'vitest'
import type { ItemQueryResponse } from '../src/api/generated/model/itemQueryResponse.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  itemQuery: vi.fn(),
  getPrefs: vi.fn(),
  startSession: vi.fn(),
  seekSession: vi.fn(),
  getItemFontUrl: vi.fn(),
  getItemSubtitleFileUrl: vi.fn(),
  getSessionFileUrl: vi.fn(),
}))
vi.mock('../src/api/capabilities.ts', () => ({ buildProfile: vi.fn() }))

const api = await import('../src/api/generated/kahawai.ts')
const { buildProfile } = await import('../src/api/capabilities.ts')
const { selectPlaybackSource, startPlaybackSession } = await import('../src/api/playback.ts')
const profile = { containers: ['mp4'], video: [{ codec: 'h264' }] } as never
const detail = (selected = 1, secondLanguage = 'jpn') =>
  ({
    id: 'film',
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
  }) as unknown as ItemQueryResponse

beforeEach(() => {
  vi.resetAllMocks()
  vi.mocked(buildProfile).mockReturnValue(profile)
})

describe('final playback selection', () => {
  test('reuses a preview only when its profile and every resolved index already agree', async () => {
    const selected = await selectPlaybackSource(detail(), [], 'movies', profile)
    expect(api.itemQuery).not.toHaveBeenCalled()
    expect(selected.sourceId).toBe(1)
    expect(selected.audioTrack).toBe(0)
  })

  test('ranks on the final source-aware profile even when every audio index is zero', async () => {
    vi.mocked(api.itemQuery).mockResolvedValue(detail(2))
    const selected = await selectPlaybackSource(detail(), [], 'movies', { containers: [] } as never)
    expect(api.itemQuery).toHaveBeenCalledWith('film', {
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
    vi.mocked(api.itemQuery).mockResolvedValue(changed)
    const selected = await selectPlaybackSource(
      detail(),
      [{ scope: '', key: 'audio.movies', value: 'jpn' }],
      'movies',
      profile,
    )
    expect(api.itemQuery).toHaveBeenCalledTimes(2)
    expect(api.itemQuery).toHaveBeenLastCalledWith('film', {
      profile,
      source_audio_tracks: { 1: 1, 2: 1 },
    })
    expect(selected.sourceId).toBe(2)
    expect(selected.audioTrack).toBe(1)
    expect(api.startSession).not.toHaveBeenCalled()
  })
})
