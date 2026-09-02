import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, test, vi, expect } from 'vitest'

import PagedGrid from '../src/components/PagedGrid.vue'

const realRect = Element.prototype.getBoundingClientRect

afterEach(() => {
  vi.unstubAllGlobals()
  Element.prototype.getBoundingClientRect = realRect
  Object.defineProperty(window, 'scrollY', { value: 0, configurable: true })
})

test('asks for later chunks as their reserved rows approach the viewport', async () => {
  vi.stubGlobal('getComputedStyle', () => ({ gridTemplateColumns: '120px 120px' }))
  vi.stubGlobal('innerHeight', 400)
  Element.prototype.getBoundingClientRect = function rect(this: Element) {
    return {
      top: this.tagName === 'LI' ? 0 : -window.scrollY,
      height: this.tagName === 'LI' ? 100 : 0,
    } as DOMRect
  }
  const wrapper = mount(PagedGrid, {
    props: { total: 250, minWidth: '120px' },
    slots: { default: ({ at }: { at: number }) => `row ${at}` },
  })
  await flushPromises()
  const needs = () => (wrapper.emitted('need') ?? []) as [number[]][]
  expect(needs().some(([chunks]) => chunks.includes(0))).toBe(true)

  Object.defineProperty(window, 'scrollY', { value: 7000, configurable: true })
  window.dispatchEvent(new Event('scroll'))
  await flushPromises()
  expect(needs().some(([chunks]) => chunks.includes(1))).toBe(true)
})
