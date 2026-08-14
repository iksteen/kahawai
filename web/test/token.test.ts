import test from 'node:test'
import assert from 'node:assert/strict'
import { refreshDelayMs } from '../src/token.ts'

const MIN = 60_000

test("the server's 15-minute lifetime refreshes a minute early", () => {
  assert.equal(refreshDelayMs(15 * MIN), 14 * MIN)
})

test('a lifetime already inside the lead time refreshes at once', () => {
  assert.equal(refreshDelayMs(30_000), 0)
})
