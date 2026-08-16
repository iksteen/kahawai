/// The one button, and the one thing about it that is not decoration.
///
/// These are CLASS assertions, which is normally the wrong test — a class name
/// is not a behaviour. Here it is the only reachable one: happy-dom does no
/// layout, and the utilities are not in a stylesheet during a unit test, so
/// `getComputedStyle` cannot answer "did these two end up on one line". The
/// regression this guards shipped and was found by eye.

import { describe, expect, test } from 'vitest'
import { mount } from '@vue/test-utils'

import Btn from '../src/components/Btn.vue'

const classes = (opts?: Record<string, unknown>, slot = 'Play') =>
  mount(Btn, { props: opts ?? {}, slots: { default: slot } })
    .get('button')
    .classes()

describe('a button holding an icon and a label', () => {
  test('lays them out in a row', () => {
    // Preflight lays every `svg` out as a block. Left as an inline-block
    // button, an icon beside a label put the icon on a line of its own and the
    // words underneath it — the watched tick, two lines tall, on every item
    // page and every season page.
    const on = classes()
    expect(on.some((c) => c === 'inline-flex' || c === 'flex')).toBe(true)
    expect(on).toContain('items-center')
  })

  test('and separates them', () => {
    // A flex row closes the word space the inline layout used to supply.
    expect(classes()).toContain('gap-[7px]')
  })
})

describe('the other two variants', () => {
  test('a mono button changes face, not size', () => {
    // `.btn.small` is two classes and `.mono` is one, so the reference never
    // rendered this a size smaller — taking 12px from `.mono` as well would
    // have shrunk a button the design does not shrink.
    const it = classes({ ghost: true, small: true, mono: true })
    expect(it).toContain('font-mono')
    expect(it).toContain('text-[13px]')
    expect(it).not.toContain('text-[12px]')
  })

  test('and an armed delete still brightens under the pointer', () => {
    // `.btn:hover` is unscoped; only `.btn.ghost:hover` opts out of it. A
    // danger button that did not respond to the pointer read as disabled.
    expect(classes({ danger: true, small: true })).toContain('hover:brightness-108')
  })

  test('and danger outranks ghost, because it replaces it', () => {
    const armed = classes({ ghost: true, danger: true, small: true })
    expect(armed).toContain('bg-warn')
    expect(armed).not.toContain('bg-transparent')
  })
})

describe('a tick that is already ticked', () => {
  test('reads as state rather than as an offer', () => {
    // Teal text on a teal-dim border, which the ghost button does not have.
    const ticked = classes({ ghost: true, on: true })
    expect(ticked).toContain('text-teal')
    expect(ticked).toContain('border-teal-dim')
  })

  test('and an unticked one still offers', () => {
    const offer = classes({ ghost: true })
    expect(offer).toContain('text-text')
    expect(offer).not.toContain('text-teal')
  })

  test('and the state belongs to the ghost, not to the primary button', () => {
    // `on` is the watched tick, which is always the secondary offer. A filled
    // teal button turning teal-on-teal would be unreadable.
    expect(classes({ on: true })).toContain('bg-teal')
  })
})
