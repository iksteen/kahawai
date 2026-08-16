/// A poster and the three things drawn over it. UI-16 and UI-22 both live in
/// this component and neither was pinned by anything until now: dropping the
/// `srcset` or the error handler passed the whole suite.

import { mount } from '@vue/test-utils'
import { describe, expect, test, vi } from 'vitest'

vi.mock('../src/api/generated/kahawai.ts', () => ({
  getItemArtworkUrl: (id: string, params?: { size?: string; v?: string }) =>
    `/api/v1/items/${id}/artwork?size=${params?.size ?? ''}&v=${params?.v ?? ''}`,
}))

const Art = (await import('../src/components/Art.vue')).default

const item = (over: Record<string, unknown> = {}) => ({
  id: 'i1',
  kind: 'movie',
  played: false,
  art_version: 7,
  resume_position_ms: null,
  resume_duration_ms: null,
  ...over,
})

describe('which poster, and at what size', () => {
  test('a card asks for both densities', () => {
    // UI-16. Without the srcset a 1× display is sent 6.25× the pixels it can
    // show, for every card on the page.
    const art = mount(Art, { props: { item: item(), size: 'card' } })
    const [one, two] = art.find('img').attributes('srcset')!.split(', ')
    expect(one).toMatch(/size=card1x&v=7 1x$/)
    expect(two).toMatch(/size=card&v=7 2x$/)
  })

  test('and the small one is the 1x, not the other way round', () => {
    // Swapped, this is worse than having no srcset at all.
    const art = mount(Art, { props: { item: item(), size: 'card' } })
    const [one, two] = art.find('img').attributes('srcset')!.split(', ')
    expect(one).toContain('card1x')
    expect(two).not.toContain('card1x')
  })

  test('a thumbnail does not, because it is already smaller than any display', () => {
    const art = mount(Art, { props: { item: item(), size: 'thumb' } })
    expect(art.find('img').attributes('srcset')).toBeUndefined()
  })

  test('the version pins the URL, so a re-matched poster is not cached for ever', () => {
    const art = mount(Art, { props: { item: item({ art_version: 7 }), size: 'card' } })
    expect(art.find('img').attributes('src')).toContain('v=7')
  })
})

describe("showing somebody else's poster", () => {
  test('an episode can wear its show’s', () => {
    // Its own is a landscape still, and in a row of portrait posters it is the
    // one thing that does not belong.
    const art = mount(Art, {
      props: { item: item({ kind: 'episode', id: 'e1' }), size: 'card', posterOf: 'show1' },
    })
    expect(art.find('img').attributes('src')).toContain('/items/show1/artwork')
  })

  test('and the version does not travel with it', () => {
    // That number describes THIS item's artwork; pinning the parent's URL with
    // the child's version is a cache key that lies.
    const art = mount(Art, {
      props: { item: item({ art_version: 7 }), size: 'card', posterOf: 'show1' },
    })
    expect(art.find('img').attributes('src')).toContain('v=')
    expect(art.find('img').attributes('src')).not.toContain('v=7')
  })
})

describe('what is drawn over it', () => {
  test('a poster that will not load is hidden, revealing the swell behind it', () => {
    // UI-22's third state. Hidden rather than emptied: an <img> with no source
    // still gets the browser's own broken-artwork mark. `invisible`, so the
    // box keeps the height the layout measured.
    const art = mount(Art, { props: { item: item(), size: 'card' } })
    expect(art.find('img').classes()).not.toContain('invisible')
    art.find('img').trigger('error')
    return art.vm.$nextTick().then(() => {
      expect(art.find('img').classes()).toContain('invisible')
    })
  })

  test('a seen item is marked', () => {
    const art = mount(Art, { props: { item: item({ played: true }), size: 'card' } })
    expect(art.find('[title="seen"]').exists()).toBe(true)
  })

  test('and what kind of thing it is', () => {
    expect(
      mount(Art, { props: { item: item({ kind: 'album' }), size: 'card' } })
        .find('[title="album"]')
        .exists(),
    ).toBe(true)
    // A show says "series", because that is the word the interface uses.
    expect(
      mount(Art, { props: { item: item({ kind: 'show' }), size: 'card' } })
        .find('[title="series"]')
        .exists(),
    ).toBe(true)
  })

  test('how far through, when the card has nowhere else to say it', () => {
    const art = mount(Art, {
      props: {
        item: item({ resume_position_ms: 300, resume_duration_ms: 1200 }),
        size: 'card',
      },
    })
    expect(art.html()).toContain('width: 25%')
  })

  test('and not when it does', () => {
    // A continue-watching card's whole text column ends in a progress bar, and
    // drawing it twice on the same card says it twice.
    const art = mount(Art, {
      props: {
        item: item({ resume_position_ms: 300, resume_duration_ms: 1200 }),
        size: 'card',
        progress: false,
      },
    })
    expect(art.html()).not.toContain('width: 25%')
  })

  test('a finished item shows the mark instead of a full bar', () => {
    const art = mount(Art, {
      props: {
        item: item({ played: true, resume_position_ms: 1200, resume_duration_ms: 1200 }),
        size: 'card',
      },
    })
    expect(art.find('[title="seen"]').exists()).toBe(true)
    expect(art.html()).not.toContain('width: 100%')
  })
})
