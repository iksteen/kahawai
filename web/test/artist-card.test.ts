import { mount } from '@vue/test-utils'
import { describe, expect, test } from 'vitest'

import ArtistCard from '../src/components/ArtistCard.vue'

const artist = (art_version: number | null) => ({
  key: 'bjork',
  name: 'Björk',
  album_count: 12,
  art_version,
})

describe('Album Artist portrait', () => {
  test('does not request an image the API says is unavailable', () => {
    const wrapper = mount(ArtistCard, {
      props: { artist: artist(null), library: 'music' },
    })

    expect(wrapper.find('img').exists()).toBe(false)
  })

  test('tries a new version after an earlier image failed', async () => {
    const wrapper = mount(ArtistCard, {
      props: { artist: artist(1), library: 'music' },
    })
    const first = wrapper.find('img')
    await first.trigger('error')
    expect(first.classes()).toContain('invisible')

    await wrapper.setProps({ artist: artist(2) })

    const second = wrapper.find('img')
    expect(second.attributes('src')).toContain('v=2')
    expect(second.classes()).not.toContain('invisible')
  })
})
