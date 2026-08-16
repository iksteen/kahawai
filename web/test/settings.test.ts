/// Settings, mounted. The page's promise is "everything here saves the moment
/// you change it", so every test here is about what happens when it does not.

import { QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

import { ApiError } from '../src/api/errors.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({ getPrefs: vi.fn(), putPref: vi.fn() }))

const { getPrefs, putPref } = await import('../src/api/generated/kahawai.ts')
const { clearNotices, notice } = await import('../src/composables/notices.ts')
const Settings = (await import('../src/views/Settings.vue')).default

const prefs = (pairs: Record<string, string>) => ({
  prefs: Object.entries(pairs).map(([key, value]) => ({ scope: '', key, value })),
})

async function open() {
  const wrapper = mount(Settings, {
    global: {
      plugins: [
        [
          VueQueryPlugin,
          { queryClient: new QueryClient({ defaultOptions: { queries: { retry: false } } }) },
        ] as [typeof VueQueryPlugin, { queryClient: QueryClient }],
      ],
    },
  })
  await flushPromises()
  return wrapper
}

beforeEach(() => {
  vi.mocked(getPrefs).mockResolvedValue(prefs({ 'audio.movies': 'en,original' }) as never)
  vi.mocked(putPref).mockResolvedValue({ ok: true } as never)
  clearNotices()
})
afterEach(() => vi.resetAllMocks())

describe('loading them', () => {
  test('a failed load shows nothing rather than defaults', async () => {
    // Rendering the page anyway with every control at its default reads as
    // "these are your settings" when the truth is "we have no idea what your
    // settings are" — worse than a blank screen, because the next thing you
    // do is change one.
    vi.mocked(getPrefs).mockRejectedValue(new ApiError(503, 'restarting'))
    const wrapper = await open()
    expect(wrapper.text()).toContain('Could not load your settings.')
    expect(wrapper.find('select').exists()).toBe(false)
    expect(wrapper.findAll('input')).toHaveLength(0)
  })

  test('and what is stored is what is shown', async () => {
    const wrapper = await open()
    expect(wrapper.text()).toContain('en')
    expect(wrapper.text()).toContain('original')
  })
})

describe('adding a language', () => {
  test('goes above the backstop and saves the whole list', async () => {
    const wrapper = await open()
    const field = wrapper.find('#audio-movies')
    await field.setValue('nl')
    await field.trigger('keydown', { key: 'Enter' })
    await flushPromises()
    expect(putPref).toHaveBeenCalledWith({
      scope: '',
      key: 'audio.movies',
      value: 'en,nl,original',
    })
  })

  test('nonsense is refused here rather than by the hub', async () => {
    const wrapper = await open()
    const field = wrapper.find('#audio-movies')
    await field.setValue('english')
    await field.trigger('keydown', { key: 'Enter' })
    await flushPromises()
    expect(putPref).not.toHaveBeenCalled()
    expect(wrapper.find('[role="alert"]').text()).toContain('Two or three letters')
    expect(field.attributes('aria-invalid')).toBe('true')
  })

  test('and one that is already there is not added twice', async () => {
    const wrapper = await open()
    const field = wrapper.find('#audio-movies')
    await field.setValue('en')
    await field.trigger('keydown', { key: 'Enter' })
    await flushPromises()
    expect(putPref).not.toHaveBeenCalled()
    // And it is not an error either.
    expect(wrapper.find('#audio-movies').attributes('aria-invalid')).toBeUndefined()
  })
})

describe('when a save is refused', () => {
  test('the value goes back to what the server holds, and says so', async () => {
    // The brief: showing an error is fine as long as the on-screen values
    // revert to what the server holds.
    vi.mocked(putPref).mockRejectedValue(new ApiError(500, 'nope'))
    const wrapper = await open()
    const field = wrapper.find('#audio-movies')
    await field.setValue('nl')
    await field.trigger('keydown', { key: 'Enter' })
    await flushPromises()

    expect(notice.value).toContain('put back')
    // 'nl' is gone from the list again.
    const list = wrapper.find('[aria-label^="Audio languages for movies"]')
    expect(list.text()).not.toContain('nl')
    expect(list.text()).toContain('en')
  })
})

describe('the bandwidth ceiling', () => {
  test('saves on leaving the box', async () => {
    const wrapper = await open()
    const field = wrapper.find('input[type="number"]')
    await field.setValue('4000')
    await field.trigger('blur')
    await flushPromises()
    expect(putPref).toHaveBeenCalledWith({ scope: '', key: 'bandwidth_kbps', value: '4000' })
  })

  test('and zero is stored the way the server stores no cap', async () => {
    // The pref is cleared rather than set to "0", so a local copy holding "0"
    // disagrees with the hub about the same key. Starting from a cap, because
    // typing 0 where there is none already changes nothing.
    vi.mocked(getPrefs).mockResolvedValue(prefs({ bandwidth_kbps: '4000' }) as never)
    const wrapper = await open()
    const field = wrapper.find('input[type="number"]')
    expect((field.element as HTMLInputElement).value).toBe('4000')

    await field.setValue('0')
    await field.trigger('blur')
    await flushPromises()
    expect(putPref).toHaveBeenCalledWith({ scope: '', key: 'bandwidth_kbps', value: '' })
  })

  test('and something that is not a number is refused rather than saved', async () => {
    const wrapper = await open()
    const field = wrapper.find('input[type="number"]')
    await field.setValue('-5')
    await field.trigger('blur')
    await flushPromises()
    expect(putPref).not.toHaveBeenCalled()
    expect(notice.value).toContain('not a number')
  })

  test('an unchanged value is not written at all', async () => {
    const wrapper = await open()
    await wrapper.find('input[type="number"]').trigger('blur')
    await flushPromises()
    expect(putPref).not.toHaveBeenCalled()
  })
})

describe('the styled-subtitle ladder', () => {
  test('is every rung, and reordering saves the whole order', async () => {
    const wrapper = await open()
    const ladder = wrapper.find('[aria-label="Styled subtitle fallbacks, in order"]')
    expect(ladder.findAll('li')).toHaveLength(3)

    await ladder.findAll('li')[2]!.trigger('keydown', { key: 'ArrowUp' })
    await flushPromises()
    expect(putPref).toHaveBeenCalledWith({
      scope: '',
      key: 'subs.ass',
      value: 'flatten,burn,overlay',
    })
  })
})

describe('the opensubtitles account', () => {
  test('and attaching one writes both halves', async () => {
    const wrapper = await open()
    await wrapper.get('#os-user').setValue('someone')
    await wrapper.get('#os-pass').setValue('a-secret')
    const save = wrapper.findAll('button').find((b) => b.text() === 'Save')!
    await save.trigger('click')
    await flushPromises()
    expect(vi.mocked(putPref).mock.calls.map((c) => c[0])).toEqual([
      { scope: '', key: 'opensubtitles.username', value: 'someone' },
      { scope: '', key: 'opensubtitles.password', value: 'a-secret' },
    ])
  })

  test('and a half-save reports which half, and shows what landed', async () => {
    // One flat failure for a half-save left the hub holding the new username
    // while the card still showed the old one — and the badge still read
    // "shared budget" for an account that was half attached.
    vi.mocked(putPref).mockImplementation((async (body: { key: string }) =>
      body.key.endsWith('password')
        ? Promise.reject(new ApiError(503, 'nope'))
        : { ok: true }) as never)
    const wrapper = await open()
    await wrapper.get('#os-user').setValue('someone')
    await wrapper.get('#os-pass').setValue('a-secret')
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Save')!
      .trigger('click')
    await flushPromises()
    expect(notice.value).toContain('password')
    // Whatever landed is what the hub has, so the card must show that: the
    // username stuck, and the password box is empty because that half did not.
    expect((wrapper.get('#os-user').element as HTMLInputElement).value).toBe('someone')
    expect((wrapper.get('#os-pass').element as HTMLInputElement).value).toBe('')
  })

  test('and Disconnect is only offered once there is something to disconnect', async () => {
    expect((await open()).findAll('button').some((b) => b.text() === 'Disconnect')).toBe(false)
    vi.mocked(getPrefs).mockResolvedValue(prefs({ 'opensubtitles.username': 'someone' }) as never)
    expect((await open()).findAll('button').some((b) => b.text() === 'Disconnect')).toBe(true)
  })
})

describe('the styled-subtitle ladder', () => {
  test('offers no way to remove a rung', async () => {
    // The order expresses priority, never removal: every rung is always
    // present, so a ✕ on each one offered something that does not exist.
    const wrapper = await open()
    const ladder = wrapper.get('[aria-label="Styled subtitle fallbacks, in order"]')
    expect(
      ladder.findAll('button').filter((b) => /^Remove /.test(b.attributes('aria-label') ?? '')),
    ).toEqual([])
  })

  test('while a language list still does', async () => {
    // The control that proves the one above is about the ladder and not about
    // `Ordered` having lost its remove button altogether.
    const wrapper = await open()
    const langs = wrapper.get('[aria-label="Audio languages for movies, in order"]')
    expect(
      langs.findAll('button').filter((b) => /^Remove /.test(b.attributes('aria-label') ?? ''))
        .length,
    ).toBeGreaterThan(0)
  })
})

describe('a language pill', () => {
  test('promotes to first choice when its name is pressed', async () => {
    // The one-press version of a drag. Stored `en,original`: pressing
    // `original` puts it in front.
    const wrapper = await open()
    const langs = wrapper.get('[aria-label="Audio languages for movies, in order"]')
    const original = langs
      .findAll('button')
      .find((b) => /Make original the first choice/.test(b.attributes('aria-label') ?? ''))!
    await original.trigger('click')
    await flushPromises()
    expect(vi.mocked(putPref)).toHaveBeenCalledWith({
      scope: '',
      key: 'audio.movies',
      value: 'original,en',
    })
  })

  test('and the backstop cannot be removed', async () => {
    const wrapper = await open()
    const langs = wrapper.get('[aria-label="Audio languages for movies, in order"]')
    expect(
      langs.findAll('button').some((b) => b.attributes('aria-label') === 'Remove original'),
    ).toBe(false)
    expect(langs.findAll('button').some((b) => b.attributes('aria-label') === 'Remove en')).toBe(
      true,
    )
  })
})

describe('the add box', () => {
  test('sits at the right edge, because the pills take the slack', async () => {
    // Measured on the original: the row and the add box end at the same x.
    // Without the grow the box sits against the last pill, wherever that ends.
    const wrapper = await open()
    const pills = wrapper.get('[aria-label="Audio languages for movies, in order"]')
    expect(pills.classes()).toContain('flex-[1_1_240px]')
    expect(wrapper.get('#audio-movies').classes()).toContain('flex-none')
  })

  test('and an empty list still holds the row open', async () => {
    // The label, the empty note and the add box are one row whether or not
    // there is anything in it.
    const wrapper = await open()
    const empty = wrapper.findAll('span').find((s) => s.text() === 'no subtitles')!
    expect(empty.classes()).toContain('flex-[1_1_240px]')
  })

  test('and offers the common languages without demanding them', async () => {
    // A combobox, not a text box. `original` is audio-only — there is no
    // "original" subtitle track — and anything already chosen is not offered
    // twice.
    const wrapper = await open()
    const audio = wrapper.get('#langs-audio-movies')
    const subs = wrapper.get('#langs-subs-movies')
    const values = (el: typeof audio) => el.findAll('option').map((o) => o.attributes('value'))
    // Stored is `en,original`, so neither is offered again.
    expect(values(audio)).not.toContain('en')
    expect(values(audio)).not.toContain('original')
    expect(values(audio)).toContain('nl')
    expect(values(subs)).not.toContain('original')
    expect(values(subs)).toContain('en')
  })
})

describe('anime numbering', () => {
  /// Two buttons that are one control: the pressed one is the current view.
  const views = (wrapper: {
    findAll: (s: string) => {
      text: () => string
      attributes: (a: string) => string | undefined
      trigger: (e: string) => Promise<void>
    }[]
  }) =>
    wrapper.findAll('button[aria-pressed]').filter((b) => ['seasons', 'native'].includes(b.text()))

  test('defaults to seasons and saves what is chosen', async () => {
    const wrapper = await open()
    const [seasons, native] = views(wrapper)
    expect(seasons!.attributes('aria-pressed')).toBe('true')
    expect(native!.attributes('aria-pressed')).toBe('false')

    await native!.trigger('click')
    await flushPromises()
    expect(putPref).toHaveBeenCalledWith({ scope: '', key: 'anime_view', value: 'native' })
  })

  test('and the choice is a pressed state, not a dimmed one', async () => {
    // The unpicked side keeps full-strength text: dimming it is how a disabled
    // control looks, and it is not disabled — it is the other half of the
    // choice.
    vi.mocked(getPrefs).mockResolvedValue(prefs({ anime_view: 'native' }) as never)
    const wrapper = await open()
    const [seasons, native] = views(wrapper)
    expect(native!.attributes('aria-pressed')).toBe('true')
    expect(seasons!.attributes('aria-pressed')).toBe('false')
  })

  test('and it lives in the Anime card, not one of its own', async () => {
    // It decides how every other screen numbers these shows, so it belongs
    // with anime rather than adrift at the bottom of the page.
    const wrapper = await open()
    const anime = wrapper
      .findAll('section')
      .find((s) => s.text().startsWith('anime') || /\banime\b/i.test(s.text().slice(0, 12)))!
    expect(anime.findAll('button[aria-pressed]').some((b) => b.text() === 'seasons')).toBe(true)
  })
})

describe('the fallback ladder', () => {
  test('says what each rung means, in the row', async () => {
    // One grid across the whole list, so every explanation starts at the same
    // place — as independent rows they began wherever each name ended, and
    // "burnt into the picture" is a lot wider than "plain text".
    const wrapper = await open()
    const rows = wrapper
      .get('[aria-label="Styled subtitle fallbacks, in order"]')
      .findAll('[role="listitem"]')
    expect(rows.length).toBeGreaterThan(0)
    for (const row of rows) {
      // The row's own `aria-label` replaces its content for a screen reader,
      // so the note has to be reachable as a description instead.
      const described = row.attributes('aria-describedby')
      expect(described).toBeTruthy()
      expect(wrapper.get(`#${described}`).text().length).toBeGreaterThan(0)
    }
  })
})
