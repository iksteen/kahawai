/// HUB-24: finding and managing an item's subtitles.
///
/// The subject of most of these is the QUOTA. The anonymous entitlement is
/// shared by everyone using the hub, so spending it is spending somebody
/// else's — and nothing else on the page would say so.

import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

import { ApiError } from '../src/api/errors.ts'
import type { TrackListing } from '../src/api/generated/model/trackListing.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  subtitleSearch: vi.fn(),
  subtitleDownload: vi.fn(),
  subtitleDelete: vi.fn(),
  putPref: vi.fn(),
  getPrefs: vi.fn(async () => ({ prefs: [] })),
}))

const api = await import('../src/api/generated/kahawai.ts')
const { notice, clearNotices } = await import('../src/composables/notices.ts')
const SubtitlePanel = (await import('../src/components/SubtitlePanel.vue')).default

const track = (over: Partial<TrackListing> = {}) =>
  ({
    id: 1,
    item_id: 'heat',
    origin: 'embedded',
    format: 'srt',
    language: 'eng',
    label: null,
    machine: false,
    derived_from: null,
    stream_index: 0,
    delivery: 'text',
    note: '',
    deletable: false,
    ...over,
  }) as TrackListing

const candidate = (over: Record<string, unknown> = {}) => ({
  file_id: 'f1',
  provider: 'opensubtitles',
  language: 'eng',
  release_name: 'Heat.1995.BluRay',
  hash_match: false,
  downloads: 4200,
  uploader: 'someone',
  rating: 8.5,
  fps: null,
  ...over,
})

const quota = (over: Record<string, unknown> = {}) => ({
  remaining: 3,
  total: 5,
  resets_in_secs: 7200,
  per_account: false,
  ...over,
})

async function panel(over: Record<string, unknown> = {}) {
  const wrapper = mount(SubtitlePanel, {
    attachTo: document.body,
    props: {
      item: { id: 'heat', title: 'Heat', parent_id: null },
      subs: [track()],
      languages: ['eng'],
      titleChoice: '',
      fps: null,
      ...over,
    },
  })
  await flushPromises()
  return wrapper
}

const press = async (wrapper: Awaited<ReturnType<typeof panel>>, label: string) => {
  await wrapper
    .findAll('button')
    .find((b) => b.text() === label)!
    .trigger('click')
  await flushPromises()
}

beforeEach(() => {
  vi.mocked(api.subtitleSearch).mockResolvedValue({
    candidates: [candidate()],
    quota: quota(),
  } as never)
  vi.mocked(api.subtitleDownload).mockResolvedValue({ track_id: 9, quota: quota() } as never)
  vi.mocked(api.subtitleDelete).mockResolvedValue({ removed: true } as never)
  vi.mocked(api.putPref).mockResolvedValue(undefined as never)
  clearNotices()
})
afterEach(() => vi.resetAllMocks())

describe('what the item already has', () => {
  test('the file’s own tracks are one line, because the player picks those', async () => {
    const wrapper = await panel({
      subs: [track({ language: 'eng' }), track({ id: 2, language: 'fra' })],
    })
    expect(wrapper.text()).toContain('2 in the file: eng, fra')
  })

  test('and a file with none says so', async () => {
    expect((await panel({ subs: [] })).text()).toContain('No subtitles in the file.')
  })

  test('and a hub-stored track says what it is DOING for this browser', async () => {
    // A stored artefact the ladder currently skips otherwise reads as the only
    // subtitle on the item.
    const wrapper = await panel({
      subs: [track({ id: 3, origin: 'ocr', delivery: 'none', note: 'ASS declined' })],
    })
    expect(wrapper.text()).toContain('ocr')
    expect(wrapper.text()).toContain('unused')
  })

  test('and the file’s own tracks are NOT in the list', async () => {
    // The player is where those get picked; this section is about managing
    // downloads, and listing twenty-six embedded tracks here buries the two
    // rows that can actually be acted on.
    const wrapper = await panel({
      subs: [
        track({ id: 1, origin: 'embedded', language: 'eng' }),
        track({ id: 2, origin: 'downloaded', language: 'fra', deletable: true }),
      ],
    })
    const rows = wrapper.findAll('section > ul > li')
    expect(rows).toHaveLength(1)
    expect(rows[0]!.text()).toContain('downloaded')
  })

  test('and only a downloaded one can be removed', async () => {
    // The other hub-stored origins are caches that rebuild themselves, so
    // removing one would be a button that undoes nothing.
    const cached = await panel({ subs: [track({ id: 3, origin: 'ocr', deletable: false })] })
    expect(cached.findAll('button').some((b) => b.text() === 'Remove')).toBe(false)

    const mine = await panel({ subs: [track({ id: 4, origin: 'downloaded', deletable: true })] })
    await press(mine, 'Remove')
    expect(api.subtitleDelete).toHaveBeenCalledWith(4)
    expect(mine.emitted('changed')).toHaveLength(1)
  })
})

describe('searching', () => {
  test('is filtered by the media type’s language preference', async () => {
    const wrapper = await panel()
    await press(wrapper, 'Find subtitles (eng)')
    expect(api.subtitleSearch).toHaveBeenCalledWith('heat', { languages: ['eng'] })
  })

  test('and nothing found offers the unfiltered search', async () => {
    vi.mocked(api.subtitleSearch).mockResolvedValue({ candidates: [], quota: quota() } as never)
    const wrapper = await panel()
    await press(wrapper, 'Find subtitles (eng)')
    expect(wrapper.text()).toContain('Nothing in eng for this file.')
    await press(wrapper, 'Search every language instead')
    expect(vi.mocked(api.subtitleSearch).mock.calls[1]![1]).toEqual({ languages: [] })
  })

  test('and a refusal is reported rather than swallowed', async () => {
    vi.mocked(api.subtitleSearch).mockRejectedValue(new ApiError(503, 'provider is away'))
    const wrapper = await panel()
    await press(wrapper, 'Find subtitles (eng)')
    expect(wrapper.text()).toContain('provider is away')
  })

  test('and the results are a dialog, not the page', async () => {
    // Twenty-five candidates shoved the sources and the attribution a screen
    // and a half down, and choosing one deserves the foreground.
    const wrapper = await panel()
    await press(wrapper, 'Find subtitles (eng)')
    expect(wrapper.find('[role="dialog"]').exists()).toBe(true)
    expect(wrapper.text()).toContain('Heat.1995.BluRay')
  })
})

describe('a candidate', () => {
  test('says when the provider matched the exact file (HUB-22)', async () => {
    vi.mocked(api.subtitleSearch).mockResolvedValue({
      candidates: [candidate({ hash_match: true })],
      quota: quota(),
    } as never)
    const wrapper = await panel()
    await press(wrapper, 'Find subtitles (eng)')
    expect(wrapper.text()).toContain('hash')
  })

  test('and warns when it was timed for a different frame rate', async () => {
    // The classic cause of progressive drift.
    vi.mocked(api.subtitleSearch).mockResolvedValue({
      candidates: [candidate({ fps: 25 })],
      quota: quota(),
    } as never)
    const wrapper = await panel({ fps: 23.976 })
    await press(wrapper, 'Find subtitles (eng)')
    expect(wrapper.text()).toContain('25 fps')
  })

  test('but not when the two agree', async () => {
    vi.mocked(api.subtitleSearch).mockResolvedValue({
      candidates: [candidate({ fps: 23.976 })],
      quota: quota(),
    } as never)
    const wrapper = await panel({ fps: 23.976 })
    await press(wrapper, 'Find subtitles (eng)')
    expect(wrapper.text()).not.toContain('fps')
  })

  test('and downloading one closes the dialog and says what happened', async () => {
    const wrapper = await panel()
    await press(wrapper, 'Find subtitles (eng)')
    await press(wrapper, 'Download')
    expect(api.subtitleDownload).toHaveBeenCalledWith('heat', {
      file_id: 'f1',
      language: 'eng',
    })
    expect(wrapper.find('[role="dialog"]').exists()).toBe(false)
    expect(notice.value).toContain('now a track on this item')
    expect(wrapper.emitted('changed')).toHaveLength(1)
  })

  test('and a refused download keeps the dialog, with the reason', async () => {
    vi.mocked(api.subtitleDownload).mockRejectedValue(
      new ApiError(409, 'the shared quota is spent', 'subtitle_quota_spent'),
    )
    const wrapper = await panel()
    await press(wrapper, 'Find subtitles (eng)')
    await press(wrapper, 'Download')
    expect(wrapper.find('[role="dialog"]').exists()).toBe(true)
    expect(wrapper.find('[role="alert"]').text()).toContain('quota is spent')
  })
})

describe('the entitlement', () => {
  test('says how much is left, and whose it is', async () => {
    const wrapper = await panel()
    await press(wrapper, 'Find subtitles (eng)')
    const text = wrapper.text().replace(/\s+/g, ' ')
    expect(text).toContain('3 of 5 downloads left today')
    expect(text).toContain('resets in 2 h')
    expect(text).toContain('shared by everyone on this server')
  })

  test('and an account’s own entitlement is not called shared', async () => {
    vi.mocked(api.subtitleSearch).mockResolvedValue({
      candidates: [candidate()],
      quota: quota({ per_account: true }),
    } as never)
    const wrapper = await panel()
    await press(wrapper, 'Find subtitles (eng)')
    expect(wrapper.text()).not.toContain('shared by everyone')
  })

  test('and a provider that does not say leaves the standing warning up', async () => {
    vi.mocked(api.subtitleSearch).mockResolvedValue({
      candidates: [candidate()],
      quota: quota({ remaining: null }),
    } as never)
    const wrapper = await panel()
    await press(wrapper, 'Find subtitles (eng)')
    expect(wrapper.text()).toContain('shared with everyone on this server unless you attach')
  })
})

describe('a standing choice for this title', () => {
  test('is shown, because it beats the language list in Settings', async () => {
    const wrapper = await panel({ titleChoice: 'fra' })
    expect(wrapper.text()).toContain('fra for this title')
  })

  test('and "off" is a choice too', async () => {
    expect((await panel({ titleChoice: 'off' })).text()).toContain('no subtitles for this title')
  })

  test('and it can be given back, or the only way is to guess where it was set', async () => {
    const wrapper = await panel({
      titleChoice: 'fra',
      item: { id: 'e1', title: 'Episode', parent_id: 'show' },
    })
    await wrapper.find('[aria-label^="Follow my language settings"]').trigger('click')
    await flushPromises()
    // On the SERIES, which is the scope the choice was made in.
    expect(api.putPref).toHaveBeenCalledWith({ scope: 'show', key: 'subs', value: '' })
    expect(wrapper.emitted('cleared')).toHaveLength(1)
  })
})

describe('the candidate dialog is a real one', () => {
  test('the focus goes into it, or nothing is announced at all', async () => {
    // `role="dialog"` is announced when the focus arrives in it, not when it is
    // inserted.
    const wrapper = await panel()
    await press(wrapper, 'Find subtitles (eng)')
    expect(wrapper.find('[role="dialog"]').element.contains(document.activeElement)).toBe(true)
  })

  test('and comes back to whatever opened it', async () => {
    const wrapper = await panel()
    const opener = wrapper.findAll('button').find((b) => b.text().startsWith('Find'))!
    ;(opener.element as HTMLElement).focus()
    await press(wrapper, 'Find subtitles (eng)')
    await wrapper.find('[aria-label="Close"]').trigger('click')
    await flushPromises()
    expect(document.activeElement).toBe(opener.element)
  })

  test('and Escape leaves it, wherever the focus is', async () => {
    // Clicking the prose in a dialog puts the focus on `<body>`, where a
    // handler bound to the dialog's own subtree never sees the key.
    const wrapper = await panel()
    await press(wrapper, 'Find subtitles (eng)')
    ;(document.activeElement as HTMLElement | null)?.blur()
    document.body.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
    await flushPromises()
    expect(wrapper.find('[role="dialog"]').exists()).toBe(false)
  })

  test('and Tab stays inside it', async () => {
    // `aria-modal` hides the background from a virtual buffer and does nothing
    // at all to the tab order.
    const wrapper = await panel()
    await press(wrapper, 'Find subtitles (eng)')
    const stops = wrapper.findAll('[role="dialog"] button, [role="dialog"] input')
    const last = stops.at(-1)!.element as HTMLElement
    last.focus()
    last.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', bubbles: true }))
    expect(document.activeElement).toBe(stops[0]!.element)
  })

  test('and the window listener goes with it', async () => {
    // A global listener per mount, and this component mounts on every item
    // page. Counted rather than inferred: an orphan handler changes nothing
    // observable on screen, which is exactly why it accumulates unnoticed.
    const added = vi.spyOn(window, 'addEventListener')
    const removed = vi.spyOn(window, 'removeEventListener')
    const wrapper = await panel()
    const on = added.mock.calls.filter((c) => c[0] === 'keydown').length
    wrapper.unmount()
    const off = removed.mock.calls.filter((c) => c[0] === 'keydown').length
    expect(on).toBeGreaterThan(0)
    expect(off).toBe(on)
    added.mockRestore()
    removed.mockRestore()
  })
})
