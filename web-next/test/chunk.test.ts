/// A missing chunk must take the page with it — once per chunk, and only while
/// reloading is still worth trying.
///
/// The hub embeds `web/dist` in its binary, so an upgrade replaces every content
/// hash under tabs that are still open. `React.lazy` caches a rejected promise
/// for the life of the page, so without this the player could never be opened
/// again in that tab and the error boundary's Try again re-rendered the same
/// rejection for ever.

import { expect, test, vi } from 'vitest'

const store = new Map<string, string>()
let reloads = 0
;(globalThis as Record<string, unknown>).sessionStorage = {
  getItem: (k: string) => store.get(k) ?? null,
  setItem: (k: string, v: string) => void store.set(k, v),
  removeItem: (k: string) => void store.delete(k),
}
;(globalThis as Record<string, unknown>).location = { reload: () => void reloads++ }

/// One page load. The module holds this load's marks in memory, so a fresh
/// import is what a reload actually is; `sessionStorage` is what survives
/// between them, and the tests below are about exactly that boundary.
async function pageLoad() {
  reloads = 0
  // A fresh module registry IS a reload: the module holds this load's marks in
  // memory, and `sessionStorage` is what survives between them. Vite cannot
  // resolve a dynamic import built from a variable, so the reset is asked for
  // by name instead.
  vi.resetModules()
  const m = await import('../src/api/chunk.ts')
  return m.loadChunk
}

const fails = () => Promise.reject(new TypeError('Failed to fetch dynamically imported module'))
const settle = () => new Promise((r) => setTimeout(r, 20))

test('a chunk that loads is passed straight through', async () => {
  store.clear()
  const loadChunk = await pageLoad()
  expect(await loadChunk('player', async () => 'module')).toBe('module')
  expect(reloads).toBe(0)
})

test('a missing chunk reloads the page and never settles', async () => {
  store.clear()
  const loadChunk = await pageLoad()
  let settled = false
  void loadChunk('player', fails).then(() => (settled = true))
  await settle()
  expect(reloads).toBe(1)
  expect(settled).toBe(false)
})

test('a reload that did not help throws rather than reloading again', async () => {
  store.clear()
  const first = await pageLoad()
  void first('player', fails)
  await settle()
  expect(reloads).toBe(1)
  // The reload lands, and the new build is missing it too.
  const second = await pageLoad()
  await await expect(second('player', fails)).rejects.toThrow()
  expect(reloads).toBe(0)
})

test('a reload that DID help leaves no mark, so the next upgrade reloads too', async () => {
  store.clear()
  const first = await pageLoad()
  void first('player', fails)
  await settle()
  // The reload lands on the new build and the chunk loads. `sessionStorage`
  // survives a reload, so leaving the mark set here is what made the SECOND
  // upgrade in a long-lived tab unrecoverable.
  const second = await pageLoad()
  await second('player', async () => 'module')
  // Upgrade two, same tab.
  void second('player', fails)
  await settle()
  expect(reloads).toBe(1)
})

test('a mark nobody came back for does not silence the next upgrade', async () => {
  // libass is only imported when an ASS track is chosen. Close the player
  // before that happens and the chunk is never re-asked — so the mark that
  // survived the reload described a question nobody asked again. Left in
  // storage it outlived the tab's whole afternoon and turned the NEXT upgrade
  // into "Subtitles for this track could not be loaded", with no reload.
  store.clear()
  const first = await pageLoad()
  void first('jassub', fails)
  await settle()
  expect(reloads).toBe(1)
  // The reload lands. Nothing asks for libass this time.
  await pageLoad()
  // A later upgrade, same tab, and now it is asked for.
  const third = await pageLoad()
  void third('jassub', fails)
  await settle()
  expect(reloads).toBe(1)
})

test("one chunk failing does not spend the other chunk's reload", async () => {
  store.clear()
  const loadChunk = await pageLoad()
  // A build with the player but no libass. One mark shared between them is a
  // loop: the player's success clears it, libass sets it and reloads, the
  // reload autoplays straight back into the same pair.
  await loadChunk('player', async () => 'player')
  void loadChunk('jassub', fails)
  await settle()
  expect(reloads).toBe(1)
  const second = await pageLoad()
  await second('player', async () => 'player')
  // Bounded on purpose: the wrong version does not reject here, it returns the
  // never-settling promise and reloads a second time — so an unbounded
  // `assert.rejects` HANGS the suite instead of failing it, which is a worse
  // way to find out.
  const verdict = await Promise.race([
    second('jassub', fails).then(
      () => 'settled',
      () => 'threw',
    ),
    new Promise<string>((r) => setTimeout(() => r('never settled'), 100)),
  ])
  expect(verdict).toBe('threw')
  expect(reloads).toBe(0)
})
