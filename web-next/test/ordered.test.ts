/// An ordered list you can rearrange. UI-12 is the keyboard half: a drag is a
/// mouse gesture and nothing else, so a list that can only be dragged cannot
/// be ordered at all without one.

import { mount } from '@vue/test-utils'
import { describe, expect, test } from 'vitest'
import { nextTick } from 'vue'

import Ordered from '../src/components/Ordered.vue'

const list = (items = ['en', 'nl', 'original'], pinned = ['original']) =>
  mount(Ordered, {
    props: { items, label: 'Audio languages', pinned },
    attachTo: document.body,
  })

describe('with a keyboard', () => {
  test('the arrows move the focused row', async () => {
    const wrapper = list()
    await wrapper.findAll('li')[1]!.trigger('keydown', { key: 'ArrowUp' })
    expect(wrapper.emitted('move')).toEqual([[1, 0]])

    await wrapper.findAll('li')[1]!.trigger('keydown', { key: 'ArrowDown' })
    expect(wrapper.emitted('move')![1]).toEqual([1, 2])
  })

  test('and they stop at the ends rather than wrapping', async () => {
    const wrapper = list()
    await wrapper.findAll('li')[0]!.trigger('keydown', { key: 'ArrowUp' })
    await wrapper.findAll('li')[2]!.trigger('keydown', { key: 'ArrowDown' })
    expect(wrapper.emitted('move')).toBeUndefined()
  })

  test('the arrows do not also scroll the page', async () => {
    const wrapper = list()
    const event = new KeyboardEvent('keydown', { key: 'ArrowUp', cancelable: true, bubbles: true })
    wrapper.findAll('li')[1]!.element.dispatchEvent(event)
    expect(event.defaultPrevented).toBe(true)
  })

  test('and focus follows the row it moved', async () => {
    // Or the next press moves a different entry, which is the one thing that
    // makes this unusable.
    const wrapper = list()
    wrapper.findAll('li')[2]!.element.focus()
    await wrapper.findAll('li')[2]!.trigger('keydown', { key: 'ArrowUp' })
    // The parent would reorder; here the list is static, so what is checked is
    // that focus went to the position it asked for.
    await nextTick()
    expect(document.activeElement).toBe(wrapper.findAll('li')[1]!.element)
    wrapper.unmount()
  })

  test('every row is reachable and says where it is', async () => {
    const wrapper = list()
    for (const [at, row] of wrapper.findAll('li').entries()) {
      expect(row.attributes('tabindex')).toBe('0')
      expect(row.attributes('aria-label')).toContain(`${at + 1} of 3`)
      expect(row.attributes('aria-label')).toContain('arrow keys')
    }
  })
})

describe('with a mouse', () => {
  test('dropping one row on another moves it there', async () => {
    // Dragging says exactly where something goes, which a swap with its
    // neighbour cannot express in one gesture.
    const wrapper = list()
    await wrapper.findAll('li')[0]!.trigger('dragstart', { dataTransfer: { setData: () => {} } })
    await wrapper.findAll('li')[2]!.trigger('drop')
    expect(wrapper.emitted('move')).toEqual([[0, 2]])
  })

  test('and a drag that ends nowhere moves nothing', async () => {
    const wrapper = list()
    await wrapper.findAll('li')[0]!.trigger('dragstart', { dataTransfer: { setData: () => {} } })
    await wrapper.findAll('li')[0]!.trigger('dragend')
    await wrapper.findAll('li')[2]!.trigger('drop')
    expect(wrapper.emitted('move')).toBeUndefined()
  })
})

describe('removing', () => {
  test('is offered for an ordinary entry', async () => {
    const wrapper = list()
    await wrapper.find('[aria-label="Remove en"]').trigger('click')
    expect(wrapper.emitted('remove')).toEqual([[0]])
  })

  test('and not for a pinned one', () => {
    // The backstop is what makes the list total: without it nothing answers
    // for a file in a language nobody named.
    expect(list().find('[aria-label="Remove original"]').exists()).toBe(false)
  })
})
