/// The two-press delete, on its own. Its one cross-instance rule — at most
/// one button armed anywhere — is the part a per-instance flag cannot state.

import { flushPromises, mount } from '@vue/test-utils'
import { expect, test } from 'vitest'

import Armed from '../src/components/Armed.vue'

const armed = (label = 'Delete') =>
  mount(Armed, { props: { label, armedLabel: `Really ${label.toLowerCase()}?` } })

test('one press arms, the second confirms', async () => {
  const button = armed()
  await button.find('button').trigger('click')
  expect(button.text()).toBe('Really delete?')
  expect(button.emitted('confirm')).toBeUndefined()

  await button.find('button').trigger('click')
  expect(button.emitted('confirm')).toHaveLength(1)
  expect(button.text()).toBe('Delete')
  button.unmount()
})

test('and arming one disarms whichever was armed before', async () => {
  // Not via blur: Safari and Firefox on macOS do not focus a button when it
  // is clicked, so the row left behind would stay armed and its next single
  // click would delete.
  const first = armed('Delete')
  const second = armed('Remove')
  await first.find('button').trigger('click')
  expect(first.text()).toBe('Really delete?')

  await second.find('button').trigger('click')
  await flushPromises()
  expect(second.text()).toBe('Really remove?')
  expect(first.text()).toBe('Delete')

  // And the disarmed one is genuinely back to a first press.
  await first.find('button').trigger('click')
  expect(first.emitted('confirm')).toBeUndefined()
  first.unmount()
  second.unmount()
})

test('an outside press or Escape disarms without relying on focus', async () => {
  const page = mount(
    {
      components: { Armed },
      template:
        '<div><Armed label="Delete" armed-label="Really delete?" /><span id="outside">elsewhere</span></div>',
    },
    { attachTo: document.body },
  )
  const control = page.findComponent(Armed)
  const button = control.find('button')

  await button.trigger('click')
  page.find('#outside').element.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true }))
  await flushPromises()
  expect(control.text()).toBe('Delete')

  // The next click is a first press again, not the deletion.
  await button.trigger('click')
  expect(control.emitted('confirm')).toBeUndefined()
  document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))
  await flushPromises()
  expect(control.text()).toBe('Delete')
  page.unmount()
})
