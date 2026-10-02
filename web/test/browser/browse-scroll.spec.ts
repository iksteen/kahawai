import { expect, test, type Page, type Route } from '@playwright/test'

/// Render the production UI with deterministic paged catalogue replies. Hold
/// the returning page until the test releases it, so scroll restoration cannot
/// accidentally pass only because the API outran the router.
async function catalogue(page: Page) {
  const errors: string[] = []
  page.on('pageerror', (error) => errors.push(String(error)))
  const token = `fixture.${Buffer.from(JSON.stringify({ username: 'fixture', admin: false })).toString('base64url')}.fixture`
  const json = (route: Route, value: unknown) => route.fulfill({ json: value })
  await page.route('**/api/v1/auth/refresh', (route) =>
    json(route, { access_token: token, expires_in: 3600 }),
  )
  await page.route('**/api/v1/prefs', (route) => json(route, { prefs: [] }))
  const libraries = [
    { id: 'fixture-films', name: 'Films', media_type: 'movies', collection_ids: [] },
    { id: 'fixture-music', name: 'Music', media_type: 'music', collection_ids: [] },
  ]
  const total = 400
  const row = (at: number, music = false) => ({
    id: `${music ? 'album' : 'film'}-${at}`,
    title: `${music ? 'Album' : 'Film'} ${String(at).padStart(3, '0')}`,
    kind: music ? 'album' : 'movie',
    media_type: music ? 'music' : 'movies',
    artist: music ? 'Artist' : null,
    year: 2000,
    copy_ids: [],
    representative_id: 'copy',
    played: false,
    metadata: { description: {}, provenance: {}, providers: {} },
  })
  const requests: number[] = []
  let release = () => {}
  let hold = false
  let requested = () => {}
  const blockNextPage = () => {
    hold = true
    const arrived = new Promise<void>((resolve) => (requested = resolve))
    return { arrived, release: () => release() }
  }
  await page.route('**/api/v1/catalogue/**', async (route) => {
    const url = new URL(route.request().url())
    const path = url.pathname
    if (path.endsWith('/libraries')) return json(route, libraries)
    if (path.endsWith('/artwork')) return route.fulfill({ status: 404 })
    if (path.endsWith('/children')) {
      return json(route, {
        children: [],
        groups: [],
        total: 0,
        offset: 0,
        limit: 200,
        watch: { played: 0, total: 0 },
      })
    }
    const music = path.includes('fixture-music')
    if (path.endsWith('/items') || path.endsWith('/artists')) {
      const offset = Number(url.searchParams.get('offset') ?? 0)
      const limit = Number(url.searchParams.get('limit') ?? 100)
      requests.push(offset)
      if (hold && offset === 0) {
        hold = false
        await new Promise<void>((resolve) => {
          release = resolve
          requested()
        })
      }
      const artists = path.endsWith('/artists')
      const reversed = url.searchParams.get('sort')?.startsWith('-')
      const rows = Array.from({ length: Math.min(limit, total - offset) }, (_, n) => {
        const at = reversed ? total - offset - n - 1 : offset + n
        return artists
          ? {
              key: `artist-${at}`,
              name: `Artist ${String(at).padStart(3, '0')}`,
              album_count: total,
            }
          : row(at, music)
      })
      return json(route, { [artists ? 'artists' : 'items']: rows, total, offset, limit })
    }
    const at = Number(/(?:film|album)-(\d+)/.exec(path)?.[1] ?? 0)
    return json(route, {
      ...row(at, music),
      copies: [],
      sources: [],
      chapters: [],
      negotiated: null,
      segments: [],
    })
  })
  return { errors, requests, blockNextPage }
}

async function depth(page: Page, top: number) {
  await page.evaluate((top) => window.scrollTo({ top }), top)
  await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(top)
  const cards = page.locator('button.card').filter({ visible: true })
  await expect(cards.first()).toBeVisible()
}

async function openCard(page: Page) {
  // Only mounted cards whose centres are in the viewport: overscan rows are
  // real DOM nodes too, and clicking one would change the depth being saved.
  await expect
    .poll(() =>
      page.locator('button.card').evaluateAll((cards) =>
        cards.some((card) => {
          const box = card.getBoundingClientRect()
          return box.y > 80 && box.bottom < window.innerHeight
        }),
      ),
    )
    .toBe(true)
  const cards = page.locator('button.card')
  for (const card of await cards.all()) {
    const box = await card.boundingBox()
    if (box && box.y > 80 && box.y + box.height < 720) {
      await card.click()
      await expect(page.locator('main h1')).toBeVisible()
      return
    }
  }
  throw new Error('No loaded card within the viewport')
}

async function returned(page: Page, top: number) {
  await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(top)
  await expect(page.locator('button.card').first()).toBeVisible()
}

test('library depth survives browser Back and the Library button with delayed pages', async ({
  page,
}) => {
  await page.setViewportSize({ width: 1000, height: 720 })
  const fixture = await catalogue(page)
  await page.goto('/app/library/fixture-films')
  await expect(page.locator('[data-scroll-ready="true"]')).toBeVisible()
  await page.getByLabel('Sort', { exact: true }).selectOption('-title')
  await expect(page.locator('.card-title').first()).toHaveText('Film 399')
  await depth(page, 12000)
  await openCard(page)
  await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(0)
  const first = fixture.blockNextPage()
  await page.goBack()
  await first.arrived
  await expect(page.locator('[data-scroll-ready="false"]')).toBeAttached()
  first.release()
  await returned(page, 12000)
  await expect(page.getByLabel('Sort', { exact: true })).toHaveValue('-title')
  expect(fixture.requests.some((offset) => offset >= 100)).toBe(true)

  await page.goForward()
  await expect(page.getByRole('button', { name: '← Library', exact: true })).toBeVisible()
  await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(0)
  await page.goBack()
  await returned(page, 12000)

  await openCard(page)
  const second = fixture.blockNextPage()
  await page.getByRole('button', { name: '← Library', exact: true }).click()
  await second.arrived
  second.release()
  await returned(page, 12000)
  await expect(page.getByLabel('Sort', { exact: true })).toHaveValue('-title')

  await page.getByLabel('Sort', { exact: true }).selectOption('title')
  await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(0)
  await expect(page.locator('.card-title').first()).toHaveText('Film 000')
  await page.goto('/app/library/fixture-films/item/film-1')
  await page.getByRole('button', { name: '← Library', exact: true }).click()
  await expect(page).toHaveURL(/\/app\/library\/fixture-films$/)
  await expect(page.locator('[data-scroll-ready="true"]')).toBeVisible()
  await expect.poll(() => page.evaluate(() => window.scrollY)).toBe(0)
  expect(fixture.errors).toEqual([])
})

test('music restores the artist list and the artist album grid', async ({ page }) => {
  await page.setViewportSize({ width: 1000, height: 720 })
  const fixture = await catalogue(page)
  await page.goto('/app/library/fixture-music')
  await expect(page.locator('[data-scroll-ready="true"]')).toBeVisible()
  await depth(page, 8000)
  await openCard(page)
  await expect(page.getByLabel('Sort albums')).toBeVisible()
  await expect(page.locator('[data-scroll-ready="true"]')).toBeVisible()
  await depth(page, 6000)
  await openCard(page)
  await page.getByRole('button', { name: '← Artist', exact: true }).click()
  await returned(page, 6000)
  await page.getByRole('button', { name: '← Library', exact: true }).click()
  await returned(page, 8000)
  expect(fixture.errors).toEqual([])
})
