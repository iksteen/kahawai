import assert from 'node:assert/strict'
import test from 'node:test'
import { SerialQueue } from '../src/serial.ts'

function deferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (reason: Error) => void
  const promise = new Promise<T>((yes, no) => {
    resolve = yes
    reject = no
  })
  return { promise, resolve, reject }
}

test('whole-state writes reach the server in click order', async () => {
  const queue = new SerialQueue()
  const first = deferred<void>()
  const started: string[] = []

  const a = queue.run(() => {
    started.push('a')
    return first.promise
  })
  const b = queue.run(async () => {
    started.push('b')
  })

  await Promise.resolve()
  assert.deepEqual(started, ['a'], 'the second write waits for the first commit')
  first.resolve()
  await Promise.all([a, b])
  assert.deepEqual(started, ['a', 'b'])
})

test('a refused write does not strand the clicks behind it', async () => {
  const queue = new SerialQueue()
  const first = deferred<void>()
  const started: string[] = []

  const refused = queue.run(() => {
    started.push('refused')
    return first.promise
  })
  const next = queue.run(async () => {
    started.push('next')
    return 'saved'
  })

  await Promise.resolve()
  first.reject(new Error('no'))
  await assert.rejects(refused)
  assert.equal(await next, 'saved')
  assert.deepEqual(started, ['refused', 'next'])
})
