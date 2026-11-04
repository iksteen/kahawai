/// The head every item page opens with. Its whole job is saying what the thing
/// IS, and nothing in it was checked: every fact on it could be deleted
/// silently.

import { mount } from '@vue/test-utils'
import { describe, expect, test } from 'vitest'

const DetailHead = (await import('../src/components/DetailHead.vue')).default

const item = (over: Record<string, unknown> = {}) => ({
  id: 'heat',
  library_id: 'films',
  kind: 'movie',
  title: 'Heat',
  year: 1995,
  art_version: 2,

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
      metadata: { release_date: '1995-12-15', rating: 8.3 },
      match_confidence: 'weak',
    })
    expect(wrapper.text()).toContain('1995-12-15')
    expect(wrapper.text()).toContain('8.3')
    expect(wrapper.text()).toContain('uncertain match')
    expect(wrapper.text()).not.toContain('seen ×')
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

  test('asks for both densities from the catalogue library', () => {
    const img = head().find('img')
    expect(img.attributes('srcset')).toContain('card1x')
    expect(img.attributes('src')).toContain('size=card')
  })

  test('changing items clears the previous poster failure', async () => {
    const wrapper = head()
    await wrapper.find('img').trigger('error')
    await wrapper.setProps({ item: item({ id: 'another-film' }) })
    expect(wrapper.find('img').attributes('src')).toContain('/items/another-film/artwork')
    expect(wrapper.find('img').classes()).not.toContain('invisible')
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

describe('additional cached artwork', () => {
  test('replacing selected images changes the actual sources, including both poster densities', async () => {
    const metadata = { artwork: ['poster-first'], logo: 'logo-first', backdrop: 'backdrop-first' }
    const wrapper = head({ metadata })
    const sources = () => wrapper.findAll('img').map((image) => image.attributes('src'))
    const previous = sources()
    const previousSrcset = wrapper.find('.art-box img').attributes('srcset')
    await wrapper.setProps({
      item: item({
        metadata: { artwork: ['poster-second'], logo: 'logo-second', backdrop: 'backdrop-second' },
      }),
    })
    const replacements = sources()
    expect(replacements).toHaveLength(previous.length)
    replacements.forEach((source, index) => expect(source).not.toBe(previous[index]))
    expect(wrapper.find('.art-box img').attributes('srcset')).not.toBe(previousSrcset)
    await wrapper.setProps({
      item: item({
        played: true,
        metadata: {
          artwork: ['poster-second'],
          logo: 'logo-second',
          backdrop: 'backdrop-second',
          rating: 9,
        },
      }),
    })
    expect(sources()).toEqual(replacements)
  })

  test('uses a backdrop and logo from this library, retaining an accessible heading', async () => {
    const wrapper = head({
      metadata: { backdrop: 'https://provider/backdrop.jpg', logo: 'https://provider/logo.png' },
    })
    expect(wrapper.find('.detail-backdrop').attributes('src')).toContain('kind=backdrop')
    expect(wrapper.html()).not.toContain('src="https://provider/')
    await wrapper.find('.title-logo img').trigger('load')
    expect(wrapper.find('h1').classes()).toContain('sr-only')
    expect(wrapper.find('h1').text()).toContain('Heat')
    const logo = wrapper.find('.title-logo img').element
    await wrapper.setProps({
      item: item({
        played: true,
        metadata: { backdrop: 'https://provider/backdrop.jpg', logo: 'https://provider/logo.png' },
      }),
    })
    expect(wrapper.find('.title-logo img').element).toBe(logo)
    expect(wrapper.find('h1').classes()).toContain('sr-only')
  })

  test('borrows the series background without replacing the episode still or title', async () => {
    const series = { id: 'show', library_id: 'films', metadata: { backdrop: 'series-background' } }
    const wrapper = head(
      { id: 'episode', kind: 'episode', metadata: { artwork: ['still'] } },
      { backgroundItem: series },
    )
    const url = wrapper.get('.detail-backdrop').attributes('src')
    expect(url).toContain('/items/show/artwork?kind=backdrop')
    expect(url).toContain('v=')
    expect(wrapper.get('.art-box img').attributes('src')).toContain(
      '/items/episode/artwork?size=card',
    )
    expect(wrapper.find('.title-logo').exists()).toBe(false)
    const img = wrapper.get('.detail-backdrop').element
    await wrapper.setProps({ backgroundItem: { ...series } })
    expect(wrapper.get('.detail-backdrop').element).toBe(img)
    await wrapper.setProps({ backgroundItem: { ...series, metadata: { backdrop: 'replacement' } } })
    expect(wrapper.get('.detail-backdrop').attributes('src')).not.toBe(url)
    await wrapper.setProps({ backgroundItem: { ...series, metadata: { banner: 'wide-banner' } } })
    expect(wrapper.get('.detail-backdrop').attributes('src')).toContain('kind=banner')
    await wrapper.setProps({ backgroundItem: undefined })
    expect(wrapper.find('.detail-backdrop').exists()).toBe(false)
  })

  test('an item background takes precedence over borrowed artwork', () => {
    const wrapper = head(
      { id: 'episode', kind: 'episode', metadata: { backdrop: 'own-background' } },
      {
        backgroundItem: {
          id: 'show',
          library_id: 'films',
          metadata: { backdrop: 'series-background' },
        },
      },
    )
    expect(wrapper.get('.detail-backdrop').attributes('src')).toContain(
      '/items/episode/artwork?kind=backdrop',
    )
  })

  test('uses a banner if there is no backdrop and keeps music compact', () => {
    expect(
      head({ metadata: { banner: 'banner' } })
        .find('.detail-backdrop')
        .attributes('src'),
    ).toContain('kind=banner')
    const album = head({ kind: 'album', metadata: { backdrop: 'backdrop', logo: 'logo' } })
    expect(album.find('.detail-backdrop').exists()).toBe(false)
    expect(album.find('.title-logo').exists()).toBe(false)
    expect(album.find('h1').classes()).not.toContain('sr-only')
  })

  test('failed images leave the title visible and a newly delivered image can retry', async () => {
    const wrapper = head({ metadata: { logo: 'first', backdrop: 'first' } })
    await wrapper.find('.title-logo img').trigger('error')
    await wrapper.find('.detail-backdrop').trigger('error')
    expect(wrapper.find('.title-logo').exists()).toBe(false)
    expect(wrapper.find('.detail-backdrop').exists()).toBe(false)
    expect(wrapper.find('h1').classes()).not.toContain('sr-only')
    await wrapper.setProps({ item: item({ metadata: { logo: 'second', backdrop: 'second' } }) })
    expect(wrapper.find('.title-logo img').exists()).toBe(true)
    expect(wrapper.find('.detail-backdrop').exists()).toBe(true)
  })

  test('a season uses its cover and keeps the season heading beside the show logo', async () => {
    const wrapper = head({ metadata: { logo: 'logo' } }, { season: 0, heading: 'Specials' })
    expect(wrapper.find('.art-box img').attributes('src')).toContain('season=0')
    expect(wrapper.find('.art-box img').attributes('srcset')).toContain('season=0')
    await wrapper.find('.title-logo img').trigger('load')
    expect(wrapper.find('h1').text()).toBe('Specials')
    expect(wrapper.find('h1').classes()).not.toContain('sr-only')
  })
})
