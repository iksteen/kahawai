import { mount } from '@vue/test-utils'
import { describe, expect, test } from 'vitest'

import ArtistCard from '../src/components/ArtistCard.vue'

const artist = (key = 'bjork') => ({
  key,
  name: 'Björk',
  album_count: 12,
})

describe('Album Artist portrait', () => {
  test('encodes artist names as one URL path component in both image sizes', () => {
    const wrapper = mount(ArtistCard, {
      props: { artist: artist('AC/DC'), library: 'music' },
    })
    const image = wrapper.find('img')
    expect(image.attributes('src')).toBe(
      '/api/v1/catalogue/libraries/music/artists/AC%2FDC/artwork?size=card',
    )
    expect(image.attributes('srcset')).toBe(
      '/api/v1/catalogue/libraries/music/artists/AC%2FDC/artwork?size=card1x 1x, /api/v1/catalogue/libraries/music/artists/AC%2FDC/artwork?size=card 2x',
    )
  })

  test('requests catalogue artwork without a legacy artwork version', async () => {
    const wrapper = mount(ArtistCard, {
      props: { artist: artist(), library: 'music' },
    })

    const image = wrapper.find('img')
    expect(image.attributes('src')).toBe(
      '/api/v1/catalogue/libraries/music/artists/bjork/artwork?size=card',
    )
    await image.trigger('error')
    expect(image.classes()).toContain('invisible')
  })

  test('tries a different artist after an earlier image failed', async () => {
    const wrapper = mount(ArtistCard, {
      props: { artist: artist(), library: 'music' },
    })
    const first = wrapper.find('img')
    await first.trigger('error')
    expect(first.classes()).toContain('invisible')

    await wrapper.setProps({ artist: artist('other') })

    const second = wrapper.find('img')
    expect(second.attributes('src')).toContain('/artists/other/artwork')
    expect(second.classes()).not.toContain('invisible')
  })
})
