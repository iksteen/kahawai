import { afterEach, describe, expect, test } from 'vitest'
import { flushPromises } from '@vue/test-utils'
import { browseScroll } from '../src/composables/scroll.ts'

const host = document.createElement('div')
document.body.append(host)
afterEach(() => host.replaceChildren())
function page(path: string, ready = false) {
  const el = document.createElement('main')
  el.dataset.scrollPage = path
  el.dataset.scrollReady = String(ready)
  host.append(el)
  return el
}

describe('waiting for the destination browse layout', () => {
  test('restoration waits for the requested page to reserve its height', async () => {
    const scroll = browseScroll()
    page('/library/other', true)
    const target = page('/library/films')
    let restored = false
    const result = scroll.ready('/library/films').then((ready) => {
      restored = ready
      return ready
    })
    await flushPromises()
    expect(restored).toBe(false)
    target.dataset.scrollReady = 'true'
    expect(await result).toBe(true)
  })

  test('an already rendered grid can restore immediately', async () => {
    page('/library/films', true)
    expect(await browseScroll().ready('/library/films')).toBe(true)
  })

  test('leaving cancels a delayed restoration without affecting the next one', async () => {
    const scroll = browseScroll()
    const old = page('/library/films')
    const first = scroll.ready('/library/films')
    scroll.cancel()
    expect(await first).toBe(false)
    const next = scroll.ready('/library/music')
    old.dataset.scrollReady = 'true'
    page('/library/music', true)
    expect(await next).toBe(true)
  })
})
