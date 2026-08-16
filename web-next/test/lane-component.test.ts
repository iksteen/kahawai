/// The lane, mounted. `lane.test.ts` covers the arithmetic; this covers the
/// wiring — that the element's numbers reach it, that the answer reaches the
/// arrows, and that the ask is emitted. All four were unpinned: keying both
/// arrows to the same edge, dropping the scroll listener, and never emitting
/// each passed the whole suite.
///
/// happy-dom does no layout, so every `scrollWidth` it reports is zero. The
/// numbers are stubbed onto the element, which is exactly the seam the domain
/// module exists to make testable.

import { mount } from '@vue/test-utils'
import { describe, expect, test } from 'vitest'
import { defineComponent, h, nextTick, ref } from 'vue'

import Lane from '../src/components/Lane.vue'

/// A lane 600 wide holding `scrollWidth`, scrolled to `scrollLeft`.
function laneAt(el: Element, scrollLeft: number, scrollWidth = 1500) {
  for (const [name, value] of [
    ['scrollLeft', scrollLeft],
    ['clientWidth', 600],
    ['scrollWidth', scrollWidth],
  ] as const) {
    Object.defineProperty(el, name, { value, configurable: true })
  }
}

/// In a parent that owns the card count, so a page arriving can be simulated
/// as what it is: more children, and nothing else changing.
function lane(cards = 3) {
  const count = ref(cards)
  const asked: true[] = []
  const wrapper = mount(
    defineComponent({
      setup() {
        return () =>
          h(
            Lane,
            { step: 450, label: 'Films', onNearEnd: () => asked.push(true) },
            {
              default: () =>
                Array.from({ length: count.value }, (_, n) => h('button', `card ${n}`)),
            },
          )
      },
    }),
  )
  return { wrapper, el: wrapper.find('[role="group"]'), count, asked }
}

const arrows = (wrapper: ReturnType<typeof lane>['wrapper']) => ({
  left: wrapper.find('[aria-label="Scroll Films left"]'),
  right: wrapper.find('[aria-label="Scroll Films right"]'),
})

describe('the arrows', () => {
  test('are dimmed on the side there is nothing to see', async () => {
    const { wrapper, el } = lane()
    laneAt(el.element, 0)
    await el.trigger('scroll')

    expect(arrows(wrapper).left.attributes('disabled')).toBeDefined()
    expect(arrows(wrapper).right.attributes('disabled')).toBeUndefined()
  })

  test('and the other way round at the end', async () => {
    // Both keyed to the same edge passed every test there was.
    const { wrapper, el } = lane()
    laneAt(el.element, 900)
    await el.trigger('scroll')

    expect(arrows(wrapper).left.attributes('disabled')).toBeUndefined()
    expect(arrows(wrapper).right.attributes('disabled')).toBeDefined()
  })

  test('are present even when they cannot move anything', async () => {
    // An arrow that disappears under the cursor hands your click to the card
    // beneath it. A disabled button still occupies the hit area.
    const { wrapper, el } = lane()
    laneAt(el.element, 0, 600)
    await el.trigger('scroll')

    expect(arrows(wrapper).left.exists()).toBe(true)
    expect(arrows(wrapper).right.exists()).toBe(true)
    expect(arrows(wrapper).left.attributes('disabled')).toBeDefined()
    expect(arrows(wrapper).right.attributes('disabled')).toBeDefined()
  })

  test('and are reachable by a keyboard', () => {
    // `display: none` until hover puts them out of the tab order entirely, so
    // the only way past the first screenful would be Tab through every card —
    // and the arrow that fetches the next page could not be reached at all.
    const { wrapper } = lane()
    for (const arrow of [arrows(wrapper).left, arrows(wrapper).right]) {
      // Faded out and revealed on hover OR focus — never `display: none`,
      // which takes it out of the tab order and the accessibility tree with it.
      expect(arrow.classes()).not.toContain('hidden')
      expect(arrow.classes()).toContain('opacity-0')
      expect(arrow.classes()).toContain('focus-visible:opacity-100')
      expect(arrow.attributes('aria-label')).toContain('Films')
    }
  })

  test('name the row they scroll', () => {
    // Nine lanes on a page, and "Scroll right" nine times names nothing.
    const { wrapper, el } = lane()
    expect(el.attributes('aria-label')).toBe('Films')
    expect(arrows(wrapper).right.attributes('aria-label')).toBe('Scroll Films right')
  })
})

describe('asking for more', () => {
  /// A lane whose contents FIT is near its end, so it asks on mount — which is
  /// right, and is what fills a shelf wider than its library. happy-dom
  /// reports every measurement as zero, so that first ask has already happened
  /// by the time a test can stub anything; these count from there.
  const asks = (recorded: true[]) => recorded.length

  test('a lane with room to scroll asks for nothing yet', async () => {
    const { el, asked } = lane()
    const before = asks(asked)
    laneAt(el.element, 0)
    await el.trigger('scroll')
    expect(asks(asked)).toBe(before)
  })

  test('and asks when the end comes within one press', async () => {
    const { el, asked } = lane()
    laneAt(el.element, 0)
    await el.trigger('scroll')
    const before = asks(asked)

    laneAt(el.element, 900)
    await el.trigger('scroll')
    expect(asks(asked)).toBe(before + 1)
  })

  test('once per width, however many times it is looked at', async () => {
    const { el, asked } = lane()
    laneAt(el.element, 0)
    await el.trigger('scroll')
    const before = asks(asked)

    laneAt(el.element, 900)
    await el.trigger('scroll')
    await el.trigger('scroll')
    await el.trigger('scroll')
    expect(asks(asked)).toBe(before + 1)
  })

  test('and the arrows are re-read once the cards arrive', async () => {
    // Appending cards changes `scrollWidth` and nothing else — not the
    // element's box — so neither a scroll event nor a ResizeObserver fires.
    // Without an update hook the right arrow stayed disabled over a lane that
    // had just grown by twenty cards, and the shelf could never ask again.
    const { wrapper, el, count } = lane()
    laneAt(el.element, 900)
    await el.trigger('scroll')
    expect(arrows(wrapper).right.attributes('disabled')).toBeDefined()

    // The page lands: twenty more cards, and the lane is no longer at its end.
    count.value = 23
    laneAt(el.element, 900, 3000)
    await nextTick()

    expect(arrows(wrapper).right.attributes('disabled')).toBeUndefined()
  })
})
