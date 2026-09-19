import { mount } from '@vue/test-utils'
import { expect, test } from 'vitest'
import Attribution from '../src/components/Attribution.vue'
import { descriptionProviders, itemProviders } from '../src/domain/attribution.ts'

test('credits all contributors once, with logo, disclaimer and direct links', () => {
  const wrapper = mount(Attribution, {
    props: { providers: ['tmdb', 'tvdb', 'tmdb', 'local', 'manual'] },
  })
  expect(wrapper.findAll('img[alt="TMDB"]')).toHaveLength(1)
  expect(wrapper.get('a[href="https://www.themoviedb.org"]').find('img').exists()).toBe(true)
  expect(wrapper.get('a[href="https://thetvdb.com"]').text()).toBe('TheTVDB')
  expect(wrapper.text()).toContain('not endorsed, certified, or otherwise approved by TMDB')
  expect(wrapper.text()).not.toContain('local')
  expect(wrapper.text()).not.toContain('manual')
})

test('AniDB is credited separately from AniList and updates with the data', async () => {
  const wrapper = mount(Attribution, { props: { providers: ['anidb'] } })
  expect(wrapper.text()).toContain('Metadata from AniDB')
  expect(wrapper.text()).not.toContain('AniList')
  await wrapper.setProps({ providers: ['anilist'] })
  expect(wrapper.text()).toContain('Metadata from AniList')
  expect(wrapper.text()).not.toContain('AniDB')
  await wrapper.setProps({ providers: [] })
  expect(wrapper.find('footer').exists()).toBe(false)
})

test('browsing combines resolved contributor names without exposing record IDs', () => {
  const attribution = descriptionProviders({
    description: {},
    provenance: { overview: 'record-a' },
    providers: { 'record-a': 'tmdb', 'record-b': 'tvdb', 'record-c': 'tmdb' },
  })
  expect(
    itemProviders([{ attribution }, null, undefined, { attribution: ['anidb', 'tmdb'] }]),
  ).toEqual(['tmdb', 'tvdb', 'anidb'])
})
