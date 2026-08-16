/// Reordering a preference list, and the write queue that saves it.
///
/// Both exist because of the same class of bug: what is on screen and what is
/// persisted disagreeing, either because a move was computed wrongly or
/// because two writes landed in the wrong order.

import { describe, expect, test } from 'vitest'

import { addAbove, moved, step } from '../src/domain/reorder.ts'
import { SerialQueue } from '../src/composables/serial.ts'

describe('moving a row', () => {
  test('says exactly where it goes, which a swap cannot', () => {
    expect(moved(['a', 'b', 'c', 'd'], 0, 2)).toEqual(['b', 'c', 'a', 'd'])
    expect(moved(['a', 'b', 'c', 'd'], 3, 1)).toEqual(['a', 'd', 'b', 'c'])
  })

  test('a move that changes nothing is not a write', () => {
    expect(moved(['a', 'b'], 1, 1)).toBeNull()
  })

  test('and an out-of-range move is refused rather than performed', () => {
    // Unbounded, an out-of-range source spliced out nothing and spliced
    // `undefined` back in: a list one longer with a hole in it, saved as the
    // new order.
    expect(moved(['a', 'b'], 5, 0)).toBeNull()
    expect(moved(['a', 'b'], 0, 5)).toBeNull()
    expect(moved(['a', 'b'], -1, 0)).toBeNull()
    expect(moved(['a', 'b'], 0, -1)).toBeNull()
  })

  test('the list it was given is left alone', () => {
    const before = ['a', 'b', 'c']
    moved(before, 0, 2)
    expect(before).toEqual(['a', 'b', 'c'])
  })
})

describe('the keyboard’s version of the same gesture', () => {
  test('moves one place at a time', () => {
    // UI-12: a drag is a mouse gesture and nothing else, so a list that can
    // only be dragged cannot be ordered without one.
    expect(step(['a', 'b', 'c'], 2, -1)).toEqual(['a', 'c', 'b'])
    expect(step(['a', 'b', 'c'], 0, 1)).toEqual(['b', 'a', 'c'])
  })

  test('and stops at the ends rather than wrapping', () => {
    expect(step(['a', 'b'], 0, -1)).toBeNull()
    expect(step(['a', 'b'], 1, 1)).toBeNull()
  })
})

describe('adding to a list with a backstop in it', () => {
  test('goes above the pin, wherever the pin sits', () => {
    // `original` resolves to the file's own language, so a language added
    // after it is never reached and the setting silently does nothing.
    expect(addAbove(['en', 'original', 'nl'], 'de', 'original')).toEqual([
      'en',
      'de',
      'original',
      'nl',
    ])
  })

  test('and the pin is not moved to the end to make room', () => {
    // It is reorderable on purpose; moving it would rewrite an order the
    // viewer chose.
    expect(addAbove(['original', 'nl'], 'de', 'original')).toEqual(['de', 'original', 'nl'])
  })

  test('a list with no pin just grows', () => {
    expect(addAbove(['en'], 'de', 'original')).toEqual(['en', 'de'])
  })
})

describe('the write queue', () => {
  /// A promise somebody else decides when to settle.
  function held<T>() {
    let settle!: (value: T) => void
    let refuse!: (why: unknown) => void
    const promise = new Promise<T>((resolve, reject) => {
      settle = resolve
      refuse = reject
    })
    return { promise, settle, refuse }
  }

  test('writes to one key commit in the order they were made', async () => {
    // Ignoring stale replies is not enough: an older request can commit after
    // a newer one and leave the persisted value opposite to the screen.
    const queue = new SerialQueue()
    const first = held<void>()
    const done: string[] = []

    const a = queue.run('subs', async () => {
      await first.promise
      done.push('a')
    })
    const b = queue.run('subs', async () => {
      done.push('b')
    })

    // The second has not run: the first is still out.
    await Promise.resolve()
    expect(done).toEqual([])

    first.settle()
    await Promise.all([a, b])
    expect(done).toEqual(['a', 'b'])
  })

  test('and a refusal does not stop the next one', async () => {
    const queue = new SerialQueue()
    const done: string[] = []
    const failed = queue.run('subs', () => Promise.reject(new Error('no')))
    const after = queue.run('subs', async () => {
      done.push('after')
    })

    await expect(failed).rejects.toThrow('no')
    await after
    expect(done).toEqual(['after'])
  })

  test('two keys do not wait for each other', async () => {
    // A slow write to one setting must not hold up every other control on the
    // page.
    const queue = new SerialQueue()
    const slow = held<void>()
    const done: string[] = []

    void queue.run('subs', async () => {
      await slow.promise
      done.push('subs')
    })
    await queue.run('audio', async () => {
      done.push('audio')
    })
    expect(done).toEqual(['audio'])

    slow.settle()
  })

  test('and the caller still sees what its own write returned', async () => {
    const queue = new SerialQueue()
    await expect(queue.run('k', async () => 'saved')).resolves.toBe('saved')
  })
})
