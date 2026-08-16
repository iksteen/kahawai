/// Saving a fetched body as a file (OPS-10's two logs).
///
/// Fetched rather than linked, because everything worth downloading here is
/// behind the bearer and a bare `<a href>` carries no Authorization header — it
/// would save the sign-in refusal instead of the log.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

import { saveAs } from '../src/api/download.ts'

let made: string[]
let revoked: string[]

beforeEach(() => {
  made = []
  revoked = []
  vi.stubGlobal('URL', {
    ...URL,
    createObjectURL: () => {
      const url = `blob:n${made.length}`
      made.push(url)
      return url
    },
    revokeObjectURL: (url: string) => revoked.push(url),
  })
})
afterEach(() => {
  vi.unstubAllGlobals()
  vi.useRealTimers()
})

describe('saving a file', () => {
  test('names it, and the anchor is in the document when it is clicked', () => {
    // A detached anchor's synthetic click has historically done nothing in
    // Firefox.
    let seen: { name: string; attached: boolean } | null = null
    const click = vi
      .spyOn(HTMLAnchorElement.prototype, 'click')
      .mockImplementation(function (this: HTMLAnchorElement) {
        seen = { name: this.download, attached: this.isConnected }
      })
    saveAs('session-s1.log', 'hello')
    expect(seen).toEqual({ name: 'session-s1.log', attached: true })
    click.mockRestore()
  })

  test('and takes the anchor back out', () => {
    const before = document.querySelectorAll('a').length
    vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {})
    saveAs('x.log', 'hello')
    expect(document.querySelectorAll('a')).toHaveLength(before)
    vi.restoreAllMocks()
  })

  test('and does not revoke the URL out from under the download', () => {
    // Revoking in the same tick pulls the object away from a download that has
    // not started reading it yet.
    vi.useFakeTimers()
    vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {})
    saveAs('x.log', 'hello')
    expect(revoked).toEqual([])
    vi.advanceTimersByTime(1)
    expect(revoked).toEqual(made)
    vi.restoreAllMocks()
  })

  test('and still cleans up when the click throws', () => {
    vi.useFakeTimers()
    vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {
      throw new Error('no')
    })
    expect(() => saveAs('x.log', 'hello')).toThrow()
    vi.advanceTimersByTime(1)
    expect(revoked).toEqual(made)
    expect(document.querySelectorAll('a')).toHaveLength(0)
    vi.restoreAllMocks()
  })
})
