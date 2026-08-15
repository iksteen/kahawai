/// The last line. A screen that throws while rendering used to take the whole
/// app with it: white page, no header, nothing to report but "it went blank".

import { mount } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { defineComponent, h, nextTick, ref } from 'vue'

import Boundary from '../src/components/Boundary.vue'

/// A component that throws while rendering, on demand.
const Bomb = defineComponent({
  props: { armed: { type: Boolean, default: true } },
  setup(props) {
    return () => {
      if (props.armed) throw new Error('the shape was not what I expected')
      return h('p', 'fine')
    }
  },
})

// The boundary logs the stack, deliberately — that is where it is readable.
// Silenced here so a passing run is not full of red.
beforeEach(() => vi.spyOn(console, 'error').mockImplementation(() => {}))
afterEach(() => vi.restoreAllMocks())

describe('a screen that throws', () => {
  test("is caught, and says what happened in the hub's own words", async () => {
    const wrapper = mount(Boundary, {
      props: { resetKey: 'a' },
      slots: { default: () => h(Bomb) },
    })
    await nextTick()
    expect(wrapper.text()).toContain('This screen stopped working.')
    // The message is kept. It is occasionally the only clue anybody gets, and
    // hiding it behind "something went wrong" would be tidier and worse.
    expect(wrapper.text()).toContain('the shape was not what I expected')
  })

  test('does not take the frame with it', async () => {
    // What the boundary is FOR: everything outside it still renders.
    const wrapper = mount(
      defineComponent({
        components: { Boundary, Bomb },
        template: '<header>kahawai~</header><Boundary reset-key="a"><Bomb /></Boundary>',
      }),
    )
    expect(wrapper.text()).toContain('kahawai~')
  })

  test('offers a retry that rebuilds the screen', async () => {
    const armed = ref(true)
    const wrapper = mount(Boundary, {
      props: { resetKey: 'a' },
      slots: { default: () => h(Bomb, { armed: armed.value }) },
    })
    await nextTick()
    expect(wrapper.text()).toContain('This screen stopped working.')

    armed.value = false
    await wrapper.find('button').trigger('click')
    expect(wrapper.text()).toBe('fine')
  })

  test('a retry against a screen still broken fails again rather than blanking', async () => {
    const wrapper = mount(Boundary, {
      props: { resetKey: 'a' },
      slots: { default: () => h(Bomb) },
    })
    await nextTick()
    await wrapper.find('button').trigger('click')
    expect(wrapper.text()).toContain('This screen stopped working.')
  })

  test('leaving the screen clears it', async () => {
    // Keyed on the SCREEN rather than the address: an autoplay handover
    // changes the URL and must not count as leaving. See `boundaryKey`.
    const armed = ref(true)
    const wrapper = mount(Boundary, {
      props: { resetKey: 'a' },
      slots: { default: () => h(Bomb, { armed: armed.value }) },
    })
    await nextTick()
    expect(wrapper.text()).toContain('This screen stopped working.')

    armed.value = false
    await wrapper.setProps({ resetKey: 'b' })
    expect(wrapper.text()).toBe('fine')
  })

  test('staying on it does not', async () => {
    // Re-rendering the same screen must not clear a latched failure, or the
    // error card flickers away on the next tick and the page is blank again.
    const wrapper = mount(Boundary, {
      props: { resetKey: 'a', away: 'Home' },
      slots: { default: () => h(Bomb) },
    })
    await nextTick()
    await wrapper.setProps({ resetKey: 'a' })
    expect(wrapper.text()).toContain('This screen stopped working.')
  })

  test('the way out is named by whoever mounted it', async () => {
    const wrapper = mount(Boundary, {
      props: { resetKey: 'a', away: 'Home' },
      slots: { default: () => h(Bomb) },
    })
    await nextTick()
    const away = wrapper.findAll('button').find((b) => b.text() === 'Home')!
    await away.trigger('click')
    expect(wrapper.emitted('away')).toHaveLength(1)
  })

  test('and is absent where there is nowhere to go', async () => {
    // The home screen's own failure: offering "Home" from home is a button
    // that does nothing.
    const wrapper = mount(Boundary, {
      props: { resetKey: 'a' },
      slots: { default: () => h(Bomb) },
    })
    await nextTick()
    expect(wrapper.findAll('button')).toHaveLength(1)
  })
})

test('a screen that does not throw is passed straight through', () => {
  const wrapper = mount(Boundary, {
    props: { resetKey: 'a' },
    slots: { default: () => h('p', 'the library') },
  })
  expect(wrapper.text()).toBe('the library')
})
