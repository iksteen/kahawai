import { expect, test, type Page } from '@playwright/test'
import { createServer } from 'node:http'

async function catalogue(page: Page, missing = false) {
  const errors: string[] = []
  page.on('pageerror', (error) => errors.push(String(error)))
  const token = `fixture.${Buffer.from(JSON.stringify({ username: 'fixture', admin: false })).toString('base64url')}.fixture`
  await page.route('**/api/v1/auth/refresh', (route) =>
    route.fulfill({ json: { access_token: token, expires_in: 3600 } }),
  )
  await page.route('**/api/v1/prefs', (route) => route.fulfill({ json: { prefs: [] } }))
  const description = {
    overview:
      'A crew and a detective cross paths in the city. Every choice brings them closer to a final confrontation.',
    logo: 'https://provider/logo.png',
    backdrop: 'https://provider/backdrop.jpg',
    rating: 8.3,
    cast: Array.from({ length: 12 }, (_, n) => ({
      id: `tmdb:${n}`,
      image: `https://provider/${n}.jpg`,
      name: n === 0 ? 'Robert De Niro' : `Actor ${n}`,
      role: n === 0 ? 'Neil McCauley' : 'Character',
    })),
  }
  const metadata = { description, provenance: {}, providers: {} }
  const child = (n: number) => ({
    id: `episode-${n}`,
    title: `Episode ${n}`,
    parent_id: 'show',
    representative_id: 'copy',
    source_count: 1,
    position: { kind: 'episode', season: 1, episode: n },
    metadata: { description: {}, provenance: {}, providers: {} },
  })
  await page.route('**/api/v1/catalogue/**', async (route) => {
    const url = new URL(route.request().url())
    const path = url.pathname
    if (path.endsWith('/libraries'))
      return route.fulfill({
        json: [{ id: 'films', name: 'Films', media_type: 'movies', collection_ids: [] }],
      })
    if (path.endsWith('/artwork')) {
      if (missing) return route.fulfill({ status: 404 })
      const logo = url.searchParams.get('kind') === 'logo'
      return route.fulfill({
        contentType: 'image/svg+xml',
        body: logo
          ? '<svg xmlns="http://www.w3.org/2000/svg" width="480" height="150"><text x="0" y="120" fill="white" font-size="130" font-family="sans-serif" font-weight="bold">HEAT</text></svg>'
          : '<svg xmlns="http://www.w3.org/2000/svg" width="960" height="540"><defs><linearGradient id="g"><stop stop-color="#296c96"/><stop offset="1" stop-color="#724f6e"/></linearGradient></defs><path fill="url(#g)" d="M0 0h960v540H0z"/><path fill="#1d3445" d="M0 400h80V100h80v300h80V240h80v160h80V60h80v340h80V180h80v220h80V120h80v280h160v140H0z"/></svg>',
      })
    }
    if (path.endsWith('/children') || path.endsWith('/seasons/1/items')) {
      return route.fulfill({
        json: {
          children: [child(1), child(2)],
          groups: [{ kind: 'episode', number: 1, total: 2, played: 0 }],
          total: 2,
          offset: 0,
          limit: 200,
          watch: {},
        },
      })
    }
    const id = /\/items\/([^/]+)/.exec(path)?.[1] ?? 'heat'
    return route.fulfill({
      json: {
        id,
        kind: id === 'show' ? 'series' : 'movie',
        title: id === 'show' ? 'The Series' : 'Heat',
        year: 1995,
        copy_ids: [],
        representative_id: 'copy',
        played: false,
        metadata,
        sources: [],
        copies: [],
        chapters: [],
        negotiated: null,
        segments: [],
      },
    })
  })
  return errors
}

test('changed metadata replaces decoded artwork without bypassing caches on ordinary refreshes', async ({
  page,
  baseURL,
}) => {
  // Playwright route interception disables the HTTP cache. Use a real HTTP
  // fixture so this catches Chromium retaining decoded images at an unchanged
  // URL even after Vue replaces their DOM nodes (private,no-cache is not enough).
  let revision = 1
  let played = false
  let reads = 0
  const images: string[] = []
  const errors: string[] = []
  page.on('pageerror', (error) => errors.push(String(error)))
  const token = `fixture.${Buffer.from(JSON.stringify({ username: 'fixture', admin: false })).toString('base64url')}.fixture`
  const server = createServer(async (request, response) => {
    const url = new URL(request.url!, baseURL)
    const path = url.pathname
    const json = (value: unknown) => {
      response.writeHead(200, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' })
      response.end(JSON.stringify(value))
    }
    if (path.endsWith('/auth/refresh')) return json({ access_token: token, expires_in: 3600 })
    if (path.endsWith('/prefs')) return json({ prefs: [] })
    if (path.endsWith('/libraries'))
      return json([{ id: 'films', name: 'Films', media_type: 'movies', collection_ids: [] }])
    if (path.endsWith('/artwork')) {
      images.push(request.url!)
      response.writeHead(200, {
        'Content-Type': 'image/svg+xml',
        'Cache-Control': 'private, no-cache',
      })
      return response.end(
        `<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><path fill="${revision === 1 ? 'red' : 'blue'}" d="M0 0h4v4H0z"/></svg>`,
      )
    }
    if (path.endsWith('/watched')) {
      played = !played
      return json({})
    }
    if (path.endsWith('/items/heat')) {
      reads++
      return json({
        id: 'heat',
        kind: 'movie',
        title: 'Heat',
        year: 1995,
        played,
        copy_ids: [],
        representative_id: 'copy',
        sources: [],
        copies: [],
        chapters: [],
        negotiated: null,
        segments: [],
        metadata: {
          description: {
            artwork: [`https://provider/poster-${revision}`],
            logo: `https://provider/logo-${revision}`,
            backdrop: `https://provider/backdrop-${revision}`,
            cast: [{ id: 'tmdb:1', name: 'Actor', image: `https://provider/photo-${revision}` }],
          },
          provenance: {},
          providers: {},
        },
      })
    }
    // Keep using the production bundle and its real CSP headers from the suite's
    // web server. Only API data and images belong to this mutable fixture.
    try {
      const upstream = await fetch(url)
      response.writeHead(
        upstream.status,
        Object.fromEntries(
          [...upstream.headers].filter(
            ([key]) =>
              !['content-length', 'content-encoding', 'transfer-encoding', 'connection'].includes(
                key,
              ),
          ),
        ),
      )
      response.end(Buffer.from(await upstream.arrayBuffer()))
    } catch {
      response.writeHead(502)
      response.end()
    }
  })
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve))
  try {
    const address = server.address()
    if (!address || typeof address === 'string') throw new Error('Artwork fixture did not bind')
    await page.goto(`http://127.0.0.1:${address.port}/app/library/films/item/heat`)
    const selectors = [
      '.title-logo img',
      '.detail-backdrop',
      '.art-box img',
      'ul[aria-label="Cast"] img',
    ]
    async function pixels() {
      return Promise.all(
        selectors.map((selector) =>
          page.locator(selector).evaluate(async (image: HTMLImageElement) => {
            await image.decode()
            const canvas = document.createElement('canvas')
            canvas.width = canvas.height = 1
            const context = canvas.getContext('2d')!
            context.drawImage(image, 0, 0, 1, 1)
            return [...context.getImageData(0, 0, 1, 1).data]
          }),
        ),
      )
    }
    await expect(page.locator('.title-logo img')).toBeVisible()
    await expect.poll(pixels).toEqual(selectors.map(() => [255, 0, 0, 255]))
    const originalImages = [...images]
    let previousReads = reads
    await page.getByRole('button', { name: 'Mark watched', exact: true }).click()
    await expect.poll(() => reads).toBeGreaterThan(previousReads)
    await expect(page.getByRole('button', { name: 'Watched', exact: true })).toBeEnabled()
    expect(images).toEqual(originalImages)

    revision = 2
    previousReads = reads
    await page.getByRole('button', { name: 'Watched', exact: true }).click()
    await expect.poll(() => reads).toBeGreaterThan(previousReads)
    await expect.poll(pixels).toEqual(selectors.map(() => [0, 0, 255, 255]))
    expect(images.slice(originalImages.length)).toHaveLength(originalImages.length)
    for (const image of images.slice(originalImages.length))
      expect(originalImages).not.toContain(image)
    expect(errors).toEqual([])
  } finally {
    server.closeAllConnections()
    await new Promise<void>((resolve, reject) =>
      server.close((error) => (error ? reject(error) : resolve())),
    )
  }
})

for (const width of [1280, 390]) {
  test(`detail artwork and failures at ${width}px`, async ({ page }, testInfo) => {
    await page.setViewportSize({ width, height: 900 })
    const errors = await catalogue(page)
    await page.goto('/app/library/films/item/heat')
    await expect(page.locator('.title-logo img')).toBeVisible()
    await expect(page.locator('.detail-backdrop')).toBeVisible()
    await expect(page.locator('h1')).toHaveText('Heat (1995)')
    await expect(page.getByText('Robert De Niro')).toBeVisible()
    await expect(page.getByRole('button', { name: '▶ Play', exact: true })).toBeVisible()
    expect(
      await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth),
    ).toBe(true)
    expect(
      await page.locator('.title-logo img').evaluate((img: HTMLImageElement) => img.naturalWidth),
    ).toBeGreaterThan(0)
    await page.screenshot({
      path: testInfo.outputPath(`detail-${width}.png`),
      fullPage: true,
      animations: 'disabled',
    })
    expect(errors).toEqual([])

    await page.unroute('**/api/v1/catalogue/**')
    const missingErrors = await catalogue(page, true)
    await page.reload()
    await expect(page.locator('.title-logo')).toHaveCount(0)
    await expect(page.locator('.detail-backdrop')).toHaveCount(0)
    await expect(page.locator('h1')).toBeVisible()
    await expect(page.locator('h1')).not.toHaveClass(/sr-only/)
    await expect(page.getByText('Neil McCauley')).toBeVisible()
    expect(
      await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth),
    ).toBe(true)
    expect(missingErrors).toEqual([])
    await page.screenshot({
      path: testInfo.outputPath(`missing-${width}.png`),
      fullPage: true,
      animations: 'disabled',
    })
  })
}

for (const width of [1280, 390]) {
  test(`series stills and season navigation at ${width}px`, async ({ page }, testInfo) => {
    await page.setViewportSize({ width, height: 900 })
    const errors = await catalogue(page)
    await page.goto('/app/library/films/item/show')
    await expect(page.locator('button img[src*="season=1"]')).toBeVisible()
    await expect(page.locator('button img[src*="episode-1/artwork"]')).toBeVisible()
    expect(
      await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth),
    ).toBe(true)
    await page.getByRole('button', { name: 'Open Season 1', exact: true }).click()
    await expect(page).toHaveURL(/\/season\/1$/)
    await expect(page.locator('h1')).toHaveText('Season 1')
    await expect(page.locator('.art-box img[src*="season=1"]')).toBeAttached()
    if (width > 540) await expect(page.locator('.art-box img[src*="season=1"]')).toBeVisible()
    await expect(page.getByRole('button', { name: 'Mark all watched', exact: true })).toBeVisible()
    expect(
      await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth),
    ).toBe(true)
    await page.screenshot({
      path: testInfo.outputPath(`season-${width}.png`),
      fullPage: true,
      animations: 'disabled',
    })
    expect(errors).toEqual([])
  })
}
