import { mount } from '@vue/test-utils'
import { expect, test } from 'vitest'

import { artworkUrl, artworkSrcSet, castArtworkUrl } from '../src/api/artwork.ts'
import CastList from '../src/components/CastList.vue'

test('artwork selections encode path segments and compose query parameters', () => {
  const url = new URL(
    artworkUrl('show/1', null, 'card', 'TV & anime', { season: 0 }),
    'https://hub',
  )
  expect(url.pathname).toBe('/api/v1/catalogue/libraries/TV%20%26%20anime/items/show%2F1/artwork')
  expect(url.searchParams.get('size')).toBe('card')
  expect(url.searchParams.get('season')).toBe('0')
  expect(artworkSrcSet('show', null, 'tv', { season: null })).toContain('season=absolute')
  expect(artworkUrl('film', null, undefined, 'films', { kind: 'logo' })).toBe(
    '/api/v1/catalogue/libraries/films/items/film/artwork?kind=logo',
  )
  expect(castArtworkUrl('film/1', 'tmdb:123', 'films', 'thumb')).toContain(
    '/film%2F1/cast/tmdb%3A123/artwork?size=thumb',
  )
  expect(artworkUrl('film')).toBe('')
})

test('selected artwork changes the request URL while keeping revisions stable across sizes', () => {
  const image = 'https://provider/first image.png?language=en&width=1000'
  const first = new URL(
    artworkUrl('film', null, 'card', 'films', { kind: 'logo', image }),
    'https://hub',
  )
  const second = new URL(
    artworkUrl('film', null, 'card', 'films', {
      kind: 'logo',
      image: 'https://provider/replacement.png',
    }),
    'https://hub',
  )
  expect(first.searchParams.get('v')).toMatch(/^[0-9a-f]{16}$/)
  expect(first.searchParams.get('v')).not.toBe(second.searchParams.get('v'))
  expect(first.searchParams.get('size')).toBe('card')
  expect(first.searchParams.get('kind')).toBe('logo')
  expect(first.href).not.toContain('provider')
  const srcset = artworkSrcSet('film', null, 'films', { image })
  for (const source of srcset.split(', ')) {
    const url = new URL(source.split(' ')[0]!, 'https://hub')
    expect(url.searchParams.get('v')).toBe(first.searchParams.get('v'))
  }
  const photo = new URL(castArtworkUrl('film', 'tmdb:123', 'films', 'thumb', image), 'https://hub')
  expect(photo.searchParams.get('v')).toBe(first.searchParams.get('v'))
})

test('cast keeps names and roles when photos are absent or fail, and retries changed photos', async () => {
  const cast = [
    { id: 'tmdb:1', image: 'https://provider/first.jpg', name: 'Actor', role: 'Character' },
    { name: 'No photo', role: 'Another character' },
    { image: 'https://provider/unknown.jpg', name: 'No identity' },
  ]
  const wrapper = mount(CastList, { props: { cast, item: 'film', library: 'films' } })
  expect(wrapper.findAll('img')).toHaveLength(1)
  expect(wrapper.find('img').attributes('src')).toContain('/cast/tmdb%3A1/artwork')
  expect(wrapper.html()).not.toContain('src="https://provider/')
  const photo = wrapper.find('img').element
  const originalSource = wrapper.find('img').attributes('src')
  await wrapper.setProps({ cast: [...cast] })
  expect(wrapper.find('img').element).toBe(photo)
  await wrapper.find('img').trigger('error')
  expect(wrapper.findAll('img')).toHaveLength(0)
  expect(wrapper.text()).toContain('Actor')
  expect(wrapper.text()).toContain('Character')
  expect(wrapper.text()).toContain('No photo')
  await wrapper.setProps({ cast: [{ ...cast[0]!, image: 'https://provider/second.jpg' }] })
  expect(wrapper.findAll('img')).toHaveLength(1)
  expect(wrapper.find('img').attributes('src')).not.toBe(originalSource)
})
