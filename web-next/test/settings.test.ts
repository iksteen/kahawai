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

describe('anime numbering', () => {
  test('defaults to seasons and saves what is chosen', async () => {
    const wrapper = await open()
    const select = wrapper.findAll('select').at(-1)!
    expect((select.element as HTMLSelectElement).value).toBe('seasons')

    await select.setValue('native')
    await flushPromises()
    expect(putPref).toHaveBeenCalledWith({ scope: '', key: 'anime_view', value: 'native' })
  })
})
