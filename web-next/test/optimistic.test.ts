/// An optimistic write that can be put back. Every test here is a race: two
/// changes in flight at once, and which value the screen should hold when one
/// of them fails.

import { afterEach, beforeEach, describe, expect, test } from 'vitest'
import { ref } from 'vue'

import { useOptimistic } from '../src/composables/optimistic.ts'
import { clearNotices, notice } from '../src/composables/notices.ts'

/// A write somebody else decides the fate of.
function held() {
  let settle!: () => void
  let refuse!: () => void
  const promise = new Promise<void>((resolve, reject) => {
    settle = resolve
    refuse = () => reject(new Error('no'))
  })
  return { write: () => promise, settle, refuse }
}

beforeEach(() => clearNotices())
afterEach(() => clearNotices())

describe('one write', () => {
  test('shows straight away and stays when it is saved', async () => {
    const value = ref('a')
    const { put } = useOptimistic(value)
    const one = held()

    const saving = put('b', one.write)
    expect(value.value).toBe('b')

    one.settle()
    await expect(saving).resolves.toBe(true)
    expect(value.value).toBe('b')
    expect(notice.value).toBe('')
  })

  test('and goes back the way it was when it is refused', async () => {
    const value = ref('a')
    const { put } = useOptimistic(value)
    const one = held()

    const saving = put('b', one.write)
    one.refuse()
    await expect(saving).resolves.toBe(false)
    expect(value.value).toBe('a')
    expect(notice.value).toContain('put back')
  })
})

describe('two writes at once', () => {
  test('an older failure does not drag back past a newer success', async () => {
    // The screen shows what the newest write asked for, and the newest write
    // succeeded — an older one failing afterwards says nothing about it.
    const value = ref('a')
    const { put } = useOptimistic(value)
    const first = held()
    const second = held()

    const one = put('b', first.write)
    const two = put('c', second.write)
    expect(value.value).toBe('c')

    second.settle()
    await two
    first.refuse()
    await one

    expect(value.value).toBe('c')
  })

  test('an older failure while a newer one is still out changes nothing', async () => {
    // The screen shows what the newest write asked for and that write has not
    // failed. Putting it back here would undo a change nobody has refused —
    // and then the newer write succeeds over the top of the undo.
    const value = ref('a')
    const { put } = useOptimistic(value)
    const first = held()
    const second = held()

    const one = put('b', first.write)
    const two = put('c', second.write)

    first.refuse()
    await one
    expect(value.value).toBe('c')

    second.settle()
    await two
    expect(value.value).toBe('c')
  })

  test('and a newer failure reverts to what the server confirmed, not to what came before it', async () => {
    // This is the case a naive revert gets wrong: the older write SUCCEEDED,
    // so 'b' is what the server holds. Reverting to the value the failing
    // write started from would go back to 'a' — past a change that was saved.
    const value = ref('a')
    const { put } = useOptimistic(value)
    const first = held()
    const second = held()

    const one = put('b', first.write)
    const two = put('c', second.write)

    first.settle()
    await one
    second.refuse()
    await two

    expect(value.value).toBe('b')
  })

  test('and both failing goes back to where it started', async () => {
    const value = ref('a')
    const { put } = useOptimistic(value)
    const first = held()
    const second = held()

    const one = put('b', first.write)
    const two = put('c', second.write)

    first.refuse()
    await one
    second.refuse()
    await two

    expect(value.value).toBe('a')
  })

  test('a third write starts from what is on screen, not from what was saved', async () => {
    const value = ref('a')
    const { put } = useOptimistic(value)
    const one = held()
    const saving = put('b', one.write)
    one.settle()
    await saving

    const two = held()
    const again = put('c', two.write)
    expect(value.value).toBe('c')
    two.refuse()
    await again
    expect(value.value).toBe('b')
  })
})

describe('while anything is out', () => {
  test('it says so, and stops saying so when the last one lands', async () => {
    const value = ref('a')
    const { put, busy } = useOptimistic(value)
    expect(busy.value).toBe(false)

    const first = held()
    const second = held()
    const one = put('b', first.write)
    const two = put('c', second.write)
    expect(busy.value).toBe(true)

    first.settle()
    await one
    expect(busy.value).toBe(true)

    second.settle()
    await two
    expect(busy.value).toBe(false)
  })
})
