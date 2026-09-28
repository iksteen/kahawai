import { mount } from '@vue/test-utils'
import { defineComponent } from 'vue'
import { expect, test } from 'vitest'
import NativeSelect from '../src/components/NativeSelect.vue'

test('selects follow model and option changes without disturbing tentative choices', async () => {
  const wrapper = mount(
    defineComponent({
      components: { NativeSelect },
      data: () => ({
        value: 1,
        options: [
          { value: 1, label: 'one' },
          { value: 2, label: 'two' },
        ],
        disabled: false,
        tick: 0,
      }),
      template: `<span>{{ tick }}</span>
      <NativeSelect :value="value" :options="options" :disabled="disabled" />`,
    }),
  )
  const el = wrapper.get('select').element
  expect(el.value).toBe('1')
  const changes: MutationRecord[] = []
  const observer = new MutationObserver((records) => changes.push(...records))
  observer.observe(el, { subtree: true, attributes: true, childList: true, characterData: true })
  el.value = '2'
  await wrapper.setData({ tick: 1 })
  expect(el.value).toBe('2')
  expect([...changes, ...observer.takeRecords()]).toEqual([])
  observer.disconnect()
  // Disabling the control ends browsing and restores the committed selection.
  await wrapper.setData({ disabled: true })
  expect(el.disabled).toBe(true)
  expect(el.value).toBe('1')
  await wrapper.setData({ value: 2 })
  await wrapper.setData({ value: 1 })
  expect(el.value).toBe('1')
  await wrapper.setData({ options: [{ value: 2, label: 'two' }] })
  expect(el.value).toBe('')
  await wrapper.setData({
    options: [
      { value: 2, label: 'two' },
      { value: 1, label: 'one' },
    ],
  })
  expect(el.value).toBe('1')
  wrapper.unmount()
})
