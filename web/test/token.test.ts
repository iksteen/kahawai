import { expect, test } from 'vitest'

import { refreshDelayMs } from '../src/domain/token.ts'

const MIN = 60_000

test("the server's 15-minute lifetime refreshes a minute early", () => {
  expect(refreshDelayMs(15 * MIN)).toBe(14 * MIN)
})

test('a lifetime already inside the lead time refreshes at once', () => {
  expect(refreshDelayMs(30_000)).toBe(0)
})

test('a lifetime already spent does not schedule into the past', () => {
  expect(refreshDelayMs(0)).toBe(0)
  expect(refreshDelayMs(-5_000)).toBe(0)
})
