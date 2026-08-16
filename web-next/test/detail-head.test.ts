/// The head every item page opens with. Its whole job is saying what the thing
/// IS, and nothing in it was checked: every fact on it could be deleted
/// silently.

import { mount } from '@vue/test-utils'
import { describe, expect, test, vi } from 'vitest'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  getItemArtworkUrl: (id: string, p?: { size?: string }) => `/art/${id}?size=${p?.size ?? ''}`,
}))

const DetailHead = (await import('../src/components/DetailHead.vue')).default

const item = (over: Record<string, unknown> = {}) => ({
  id: 'heat',
  kind: 'movie',
  title: 'Heat',
  year: 1995,
  art_version: 2,
  play_count: 0,
  duration_ms: 170 * 60_000,
  resume_position_ms: null,
  resume_duration_ms: null,
  metadata: null,
  ...over,
})

const head = (over: Record<string, unknown> = {}, props: Record<string, unknown> = {}) =>
  mount(DetailHead, { props: { item: item(over), subline: 'a subline', ...props } })

describe('what it says', () => {
  test('the title, the year and the running time', () => {
    const wrapper = head()
    expect(wrapper.find('h1').text()).toContain('Heat')
    expect(wrapper.find('h1').text()).toContain('1995')
    expect(wrapper.text()).toContain('2 h 50 min')
  })

  test('and the facts everybody checks', () => {
    const wrapper = head({
      play_count: 3,
      metadata: { premiered: '1995-12-15', rating: 8.3, confidence: 'weak' },
    })
    expect(wrapper.text()).toContain('1995-12-15')
    expect(wrapper.text()).toContain('8.3')
    expect(wrapper.text()).toContain('uncertain match')
    expect(wrapper.text()).toContain('seen ×3')
  })

  test('and none of them when there is nothing to say', () => {
    const wrapper = head()
    expect(wrapper.text()).not.toContain('uncertain')
    expect(wrapper.text()).not.toContain('seen ×')
    expect(wrapper.text()).not.toContain('★')
  })

  test('the overview, when there is one', () => {
    expect(head({ metadata: { overview: 'A crew, a cop.' } }).text()).toContain('A crew, a cop.')
  })
})

describe('how far through it is', () => {
  test('drawn when it has been started', () => {
    const wrapper = head({ resume_position_ms: 300, resume_duration_ms: 1200 })
    expect(wrapper.html()).toContain('width: 25%')
  })

  test('and not when it has not', () => {
    expect(head().html()).not.toContain('width:')
  })

  test('and the caller can say there is nowhere for it', () => {
    // A series has no progress of its own — its progress is the season counts
    // under it.
    const wrapper = head({ resume_position_ms: 300, resume_duration_ms: 1200 }, { progress: null })
    expect(wrapper.html()).not.toContain('width: 25%')
  })
})

describe('the artwork', () => {
  test('follows what the artwork is', () => {
    // A track's is its album's square sleeve, an episode's a 16:9 still.
    expect(head({ kind: 'album' }).find('img').attributes('style')).toContain('1')
    expect(head({ kind: 'episode' }).find('img').attributes('style')).toContain('16 / 9')
    expect(head().find('img').attributes('style')).toContain('2 / 3')
  })

  test('asks for both densities, and is pinned to its version', () => {
    const img = head().find('img')
    expect(img.attributes('srcset')).toContain('card1x')
    expect(img.attributes('src')).toContain('size=card')
  })

  test('and one that will not load is hidden, revealing the swell', () => {
    const wrapper = head()
    expect(wrapper.find('img').classes()).not.toContain('invisible')
    wrapper.find('img').trigger('error')
    return wrapper.vm.$nextTick().then(() => {
      expect(wrapper.find('img').classes()).toContain('invisible')
    })
  })
})
