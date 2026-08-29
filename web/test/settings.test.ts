/// Settings, mounted. The page's promise is "everything here saves the moment
/// you change it", so every test here is about what happens when it does not.

import { QueryClient, VueQueryPlugin } from '@tanstack/vue-query'
import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

import { ApiError } from '../src/api/errors.ts'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  accountOpensubtitles: vi.fn(),
  deleteAccountOpensubtitles: vi.fn(),
  getPrefs: vi.fn(),
  putPref: vi.fn(),
  setAccountOpensubtitles: vi.fn(),
}))

const {
  accountOpensubtitles,
  deleteAccountOpensubtitles,
  getPrefs,
  putPref,
  setAccountOpensubtitles,
} = await import('../src/api/generated/kahawai.ts')
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
  vi.mocked(accountOpensubtitles).mockResolvedValue({ configured: false } as never)
  vi.mocked(setAccountOpensubtitles).mockResolvedValue({ ok: true } as never)
  vi.mocked(deleteAccountOpensubtitles).mockResolvedValue({ ok: true } as never)
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

describe('loudness normalization', () => {
  test('defaults to encoded audio and stores force mode', async () => {
    const wrapper = await open()
    const field = wrapper.get('#loudness-normalization')
    expect((field.element as HTMLSelectElement).value).toBe('')

    await field.setValue('force')
    await flushPromises()
    expect(putPref).toHaveBeenCalledWith({
      scope: '',
      key: 'loudness_normalization',
      value: 'force',
    })
  })

  test('shows a stored opt-out and clears it back to the default', async () => {
    vi.mocked(getPrefs).mockResolvedValue(prefs({ loudness_normalization: 'off' }) as never)
    const wrapper = await open()
    const field = wrapper.get('#loudness-normalization')
    expect((field.element as HTMLSelectElement).value).toBe('off')

    await field.setValue('')
    await flushPromises()
    expect(putPref).toHaveBeenCalledWith({
      scope: '',
      key: 'loudness_normalization',
      value: '',
    })
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
      key: 'ass_order',
      value: 'flatten,burn,overlay',
    })
  })
})

describe('the opensubtitles account', () => {
  test('and attaching one sends the pair exactly as typed, then empties the form', async () => {
    // Padded on both sides: the account is the account holder's to compose,
    // and neither this form nor the hub trims it.
    const wrapper = await open()
    await wrapper.get('#os-user').setValue(' someone ')
    await wrapper.get('#os-pass').setValue('  a-secret  ')
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Save')!
      .trigger('click')
    await flushPromises()
    expect(setAccountOpensubtitles).toHaveBeenCalledWith({
      username: ' someone ',
      password: '  a-secret  ',
    })
    // Nothing to keep them for: the hub will not read either back, so a filled
    // box after a save would be showing a value only this tab remembers.
    expect((wrapper.get('#os-user').element as HTMLInputElement).value).toBe('')
    expect((wrapper.get('#os-pass').element as HTMLInputElement).value).toBe('')
  })

  test('and a refused save says so and keeps what was typed', async () => {
    vi.mocked(setAccountOpensubtitles).mockRejectedValue(new ApiError(503, 'restarting'))
    const wrapper = await open()
    await wrapper.get('#os-user').setValue('someone')
    await wrapper.get('#os-pass').setValue('a-secret')
    await wrapper
      .findAll('button')
      .find((b) => b.text() === 'Save')!
      .trigger('click')
    await flushPromises()
    expect(notice.value).toBeTruthy()
    // Clearing here would cost the password a second time for a save that
    // never happened.
    expect((wrapper.get('#os-user').element as HTMLInputElement).value).toBe('someone')
    expect((wrapper.get('#os-pass').element as HTMLInputElement).value).toBe('a-secret')
  })

  test('and the hub says whether one is attached, never which', async () => {
    let wrapper = await open()
    expect(wrapper.findAll('button').some((b) => b.text() === 'Disconnect')).toBe(false)
    expect(wrapper.get('#os-user').attributes('placeholder')).toContain('username')

    vi.mocked(accountOpensubtitles).mockResolvedValue({ configured: true } as never)
    wrapper = await open()
    expect(wrapper.findAll('button').some((b) => b.text() === 'Disconnect')).toBe(true)
    // The name is not in the answer and so cannot be on the screen; the field
    // says an account is there and offers to replace it.
    expect(wrapper.get('#os-user').attributes('placeholder')).toBe(
      'account configured — enter to replace',
    )
    expect((wrapper.get('#os-user').element as HTMLInputElement).value).toBe('')
  })

  test('and a read it could not make says so rather than "no account"', async () => {
    // Defaulting the failed read to false claimed the account was not
    // attached — while the viewer's searches were failing for the same reason
    // the read did, which is a credential the hub cannot open.
    vi.mocked(accountOpensubtitles).mockRejectedValue(new ApiError(500, 'boom'))
    const wrapper = await open()
    expect(wrapper.text()).toContain('unknown')
    expect(wrapper.findAll('button').some((b) => b.text() === 'Disconnect')).toBe(false)
    // The rest of the page is unaffected: one card's read failing is not the
    // settings failing.
    expect(wrapper.text()).not.toContain('Could not load your settings.')
    expect(wrapper.find('#os-user').exists()).toBe(true)
  })

  test('and disconnecting is asked twice, then asks the hub rather than saving a blank', async () => {
    vi.mocked(accountOpensubtitles).mockResolvedValue({ configured: true } as never)
    const wrapper = await open()
    const press = async (label: string) => {
      await wrapper
        .findAll('button')
        .find((b) => b.text() === label)!
        .trigger('click')
      await flushPromises()
    }
    // One press arms it. The hub will not read the account back, so a stray
    // press costs a trip to opensubtitles.com, not a glance at the screen.
    await press('Disconnect')
    expect(deleteAccountOpensubtitles).not.toHaveBeenCalled()

    await press('Really disconnect?')
    expect(deleteAccountOpensubtitles).toHaveBeenCalled()
    expect(setAccountOpensubtitles).not.toHaveBeenCalled()
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
