import { strict as assert } from 'node:assert'
import { test } from 'node:test'
import { notify, onNotice } from '../src/toast.ts'

test('a notice reaches the mounted host', () => {
  const seen: string[] = []
  onNotice((m) => seen.push(m))
  notify('could not save')
  assert.deepEqual(seen, ['could not save'])
  onNotice(null)
})

test('with no host mounted, notifying is a no-op rather than a throw', () => {
  onNotice(null)
  // The login screen and the boot phase have nowhere to put a notice.
  // Reporting a failure there must not become a second failure.
  assert.doesNotThrow(() => notify('nowhere to show this'))
})

test('the newest host is the only one that hears', () => {
  const first: string[] = []
  const second: string[] = []
  onNotice((m) => first.push(m))
  onNotice((m) => second.push(m))
  notify('once')
  // Not two toasts saying the same thing: React strict mode mounts the
  // shell twice in development, and both registrations would otherwise
  // stay live.
  assert.deepEqual(first, [])
  assert.deepEqual(second, ['once'])
  onNotice(null)
})
