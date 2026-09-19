import { expect, test } from '@playwright/test'
import AxeBuilder from '@axe-core/playwright'

test('compose libraries from a real mediahost, persist edits and revoke grants on deletion', async ({
  page,
  request,
}) => {
  const headers = { Authorization: `Bearer ${process.env.KAHAWAI_MEDIADB_UI_TOKEN}` }
  const catalogue = async () => {
    const response = await request.get('/api/v1/catalogue/libraries', { headers })
    expect(response.ok()).toBe(true)
    return (await response.json()) as { id: string; name: string; collection_ids: string[] }[]
  }
  const collectionResponse = await request.get('/admin/v1/catalogue/collections', { headers })
  expect(collectionResponse.ok()).toBe(true)
  const collections = (await collectionResponse.json()) as { id: string; remote_id: string }[]
  const id = collections.find((c) => c.remote_id === 'movies')!.id
  const retiredBrowse: string[] = []
  page.on('request', (request) => {
    if (/\/api\/v1\/(libraries|items|artists|up-next)(?:[/?]|$)/.test(request.url()))
      retiredBrowse.push(request.url())
  })
  const errors: string[] = []
  page.on('pageerror', (error) => errors.push(error.message))
  await page.goto('/app/admin')
  await page.getByLabel('Username', { exact: true }).fill('fixture')
  await page.getByLabel('Password', { exact: true }).fill('fixture-password')
  await page.getByRole('button', { name: 'Sign in', exact: true }).click()
  await page.getByRole('tab', { name: 'Libraries', exact: true }).click()
  const composer = page.getByRole('region', { name: 'Libraries', exact: true })
  await expect(composer.getByText('ingestion-fixture/movies', { exact: false })).toBeVisible()
  await composer.getByLabel('Name', { exact: true }).fill('Browser films')
  await composer.getByRole('checkbox').check()
  await composer.getByRole('button', { name: 'Create', exact: true }).click()
  await expect(composer.getByRole('heading', { name: 'Browser films movies' })).toBeVisible()
  const created = (await catalogue()).find((row) => row.name === 'Browser films')!
  expect(created.collection_ids).toEqual([id])
  for (const [label, deep] of [
    ['Rescan', false],
    ['Deep rescan', true],
  ] as const) {
    const response = page.waitForResponse(
      (r) =>
        r.request().method() === 'POST' &&
        r.url().includes(`/catalogue/libraries/${created.id}/refresh?deep=${deep}`),
    )
    await composer.getByRole('button', { name: `${label} Browser films`, exact: true }).click()
    const result = await response
    expect(result.status()).toBe(200)
    expect(await result.json()).toEqual({ asked: 1, offline: 0, unsupported: 0 })
    await expect(
      composer.getByRole('button', { name: 'Rescan Browser films', exact: true }),
    ).toBeEnabled()
  }

  await composer.getByRole('button', { name: 'Edit collections in Browser films' }).click()
  const editor = composer.locator('form').nth(1)
  await editor.getByRole('checkbox').uncheck()
  await editor.getByRole('button', { name: 'Cancel', exact: true }).click()
  expect((await catalogue()).find((row) => row.id === created.id)!.collection_ids).toEqual([id])
  await composer.getByRole('button', { name: 'Edit collections in Browser films' }).click()
  await editor.getByRole('checkbox').uncheck()
  await editor.getByRole('button', { name: 'Save collections' }).click()
  await expect(composer.getByText('No collections assigned.')).toBeVisible()
  expect((await catalogue()).find((row) => row.id === created.id)!.collection_ids).toEqual([])
  await page.reload()
  await page.getByRole('tab', { name: 'Libraries', exact: true }).click()
  await expect(composer.getByText('No collections assigned.')).toBeVisible()
  await composer.getByRole('button', { name: 'Edit collections in Browser films' }).click()
  await editor.getByRole('checkbox').check()
  await editor.getByRole('button', { name: 'Save collections' }).click()
  await expect(
    composer.getByRole('button', { name: 'Edit collections in Browser films' }),
  ).toBeVisible()
  expect((await catalogue()).find((row) => row.id === created.id)!.collection_ids).toEqual([id])
  expect(
    (await new AxeBuilder({ page }).include('[aria-label="Libraries"]').analyze()).violations,
  ).toEqual([])
  const assertAligned = async () => {
    // Measure both controls in one layout snapshot, including after a resize.
    await expect
      .poll(() =>
        composer.evaluate((root) => {
          const type = root.querySelector('#new-library-type')!.getBoundingClientRect()
          const name = root
            .querySelector('input[placeholder="e.g. Films"]')!
            .getBoundingClientRect()
          return [type.y - name.y, type.height - name.height]
        }),
      )
      .toEqual([0, 0])
  }
  await assertAligned()
  await page.setViewportSize({ width: 390, height: 844 })
  await assertAligned()
  await expect
    .poll(() => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth))
    .toBe(true)
  await page.screenshot({ path: '/tmp/kahawai-mediadb-composer-mobile.png', fullPage: true })
  await page.setViewportSize({ width: 1280, height: 900 })
  await page.screenshot({ path: '/tmp/kahawai-mediadb-composer-desktop.png', fullPage: true })
  // Import and enrichment are independent. Wait for the fixture evidence before
  // opening its review snapshot, instead of racing the background worker.
  await expect
    .poll(
      async () => {
        const response = await request.get(`/api/v1/catalogue/libraries/${created.id}/items`, {
          headers,
        })
        const rows = (await response.json()).items as {
          metadata: { description: { overview?: string } }
        }[]
        return rows[0]?.metadata.description.overview
      },
      { timeout: 30000 },
    )
    .toBe('Metadata from the remote mediahost.')
  // Each physical copy must have a current candidate snapshot before opening
  // matching; the first copy's overview can arrive while the other is refreshing.
  await expect
    .poll(
      async () => {
        const response = await request.get(`/api/v1/catalogue/libraries/${created.id}/items`, {
          headers,
        })
        const rows = (await response.json()).items as { copy_ids: string[] }[]
        const snapshots = await Promise.all(
          rows
            .flatMap((row) => row.copy_ids)
            .map(async (copy) => {
              const response = await request.get(`/admin/v1/enrich/items/${copy}`, { headers })
              const detail = await response.json()
              return (
                detail.input.selected &&
                detail.candidates.some(
                  (candidate: { id: string }) => candidate.id === detail.input.selected[0],
                )
              )
            }),
        )
        return snapshots.length > 0 && snapshots.every(Boolean)
      },
      { timeout: 30000 },
    )
    .toBe(true)
  await expect(page.getByRole('tab', { name: 'Enrichment', exact: true })).toHaveCount(0)
  await page.goto(`/app/library/${created.id}`)
  await expect(page.getByRole('button', { name: /No metadata match.*Dark City/i })).toHaveCount(0)
  await expect(
    page.getByRole('button', { name: /Re-match metadata.*Dark City/i }).first(),
  ).toHaveClass(/opacity-0/)
  await page
    .getByRole('button', { name: /metadata.*Dark City/i })
    .first()
    .click({ force: true })
  const review = page.getByRole('dialog')
  await expect(review.getByRole('heading', { name: 'Match “Dark City” (1998)' })).toBeVisible()
  await expect(review.locator('#match-provider')).toHaveCount(0)
  const copyId = await review.getByLabel('Collection copy', { exact: true }).inputValue()
  const input = async () => {
    const response = await request.get(`/admin/v1/enrich/items/${copyId}`, { headers })
    expect(response.ok()).toBe(true)
    return (await response.json()).input as { library_item_id: string; manual: boolean }
  }
  const originalId = (await input()).library_item_id
  const matchArt = review.locator('img').first()
  await expect
    .poll(() => matchArt.evaluate((image: HTMLImageElement) => image.naturalWidth))
    .toBeGreaterThan(0)
  await review.locator('ul.grid button').first().click()
  await expect(review).not.toBeVisible()
  expect((await input()).manual).toBe(true)
  await page.reload()
  await page
    .getByRole('button', { name: /metadata.*Dark City/i })
    .first()
    .click({ force: true })
  await expect(review.getByRole('button', { name: 'Confirm current', exact: true })).toHaveCount(0)
  await review.getByRole('button', { name: 'Use automatic matching', exact: true }).click()
  await expect(review).not.toBeVisible()
  expect((await input()).manual).toBe(false)
  await page
    .getByRole('button', { name: /metadata.*Dark City/i })
    .first()
    .click({ force: true })
  await page.setViewportSize({ width: 390, height: 844 })
  await expect
    .poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth))
    .toBe(true)
  await page.screenshot({ path: '/tmp/kahawai-restored-match-mobile.png', fullPage: true })
  await page.setViewportSize({ width: 1280, height: 900 })
  await page.screenshot({ path: '/tmp/kahawai-restored-match-desktop.png', fullPage: true })
  expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([])
  await expect(review.getByText('Create a library item', { exact: true })).toHaveCount(0)
  await expect(review.getByRole('button', { name: 'Dark City · 1998', exact: true })).toHaveCount(0)
  await review.getByRole('button', { name: 'Close', exact: true }).click()
  await expect(review).not.toBeVisible()
  expect((await input()).library_item_id).toBe(originalId)
  // The viewer and jump menu share the new catalogue, including a newly created library.
  await page.goto('/app/')
  await expect(page.getByText('latest added', { exact: true }).first()).toBeVisible()
  await page.screenshot({ path: '/tmp/kahawai-restored-home.png', fullPage: true })
  await page.getByRole('button', { name: 'Browser films', exact: true }).click()
  await expect(page.getByRole('heading', { name: 'Browser films', exact: true })).toBeVisible()
  await page
    .getByRole('button', { name: /Dark City/ })
    .filter({ has: page.locator('img') })
    .click()
  await expect(page.getByText('Metadata from the remote mediahost.', { exact: true })).toBeVisible()
  await page.reload()
  await expect(page.getByRole('heading', { name: 'Dark City (1998)', exact: true })).toBeVisible()
  const sources = page
    .locator('section')
    .filter({ has: page.getByRole('heading', { name: 'Sources', exact: true }) })
  await expect(sources.getByRole('listitem')).toHaveCount(2)
  await expect(sources.getByText('Dark.City.1998.mkv', { exact: true })).toHaveCount(2)
  await expect(sources.getByText('h264 64p', { exact: true })).toHaveCount(2)
  await expect(page.getByRole('button', { name: /Play$/ })).toBeEnabled()
  await page.screenshot({ path: '/tmp/kahawai-restored-detail.png', fullPage: true })
  await sources.getByRole('button', { name: /match/i }).first().click()
  await expect(review.getByText('ingestion-fixture · movies', { exact: true })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(review).not.toBeVisible()
  await page.getByRole('button', { name: 'kahawai~', exact: true }).click()
  await page.getByRole('menuitem', { name: 'Browser films', exact: true }).click()
  await expect(page.getByRole('heading', { name: 'Browser films', exact: true })).toBeVisible()
  // The original grid retains poster geometry and filters through the new API.
  const poster = page
    .getByRole('button', { name: /Dark City/ })
    .filter({ has: page.locator('img') })
  await expect(poster).toBeVisible()
  await page.getByRole('combobox', { name: 'Sort', exact: true }).selectOption('added')
  await expect(poster).toBeVisible()
  await page.getByRole('textbox', { name: 'Filter this library', exact: true }).fill('missing')
  await expect(poster).toHaveCount(0)
  await page.getByRole('textbox', { name: 'Filter this library', exact: true }).fill('City')
  await expect(poster).toBeVisible()
  await page.getByRole('textbox', { name: 'Filter this library', exact: true }).fill('')
  await expect
    .poll(() => poster.locator('img').evaluate((img: HTMLImageElement) => img.naturalWidth))
    .toBeGreaterThan(0)
  const art = await poster.locator('img').boundingBox()
  expect(art!.height / art!.width).toBeCloseTo(1.5, 1)
  await page.setViewportSize({ width: 390, height: 844 })
  await expect
    .poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth))
    .toBe(true)
  expect((await new AxeBuilder({ page }).analyze()).violations).toEqual([])
  await page.screenshot({ path: '/tmp/kahawai-catalogue-navigation.png', fullPage: true })
  expect(retiredBrowse).toEqual([])
  await page.setViewportSize({ width: 1280, height: 900 })
  await page.goto('/app/admin')
  // Grant the actual media library through the UI; deleting it must revoke that grant.
  const userResponse = await request.post('/admin/v1/users', {
    headers,
    data: { username: 'viewer', password: 'viewer-password-long' },
  })
  expect(userResponse.ok()).toBe(true)
  await page.getByRole('tab', { name: 'Users & grants' }).click()
  const viewer = page
    .getByRole('listitem')
    .filter({ has: page.getByText('viewer', { exact: true }) })
  await viewer.getByRole('button', { name: 'all libraries', exact: true }).click()
  await viewer.getByRole('button', { name: 'Browser films', exact: true }).click()
  const users = async () =>
    (await (await request.get('/admin/v1/users', { headers })).json()).users as {
      username: string
      libraries: string[]
    }[]
  await expect
    .poll(async () => (await users()).find((row) => row.username === 'viewer')!.libraries)
    .toEqual([created.id])
  await page.getByRole('tab', { name: 'Libraries', exact: true }).click()
  await composer.getByRole('button', { name: 'Delete Browser films', exact: true }).click()
  expect((await catalogue()).some((row) => row.id === created.id)).toBe(true)
  await composer
    .getByRole('button', {
      name: 'Really delete Browser films?',
      exact: true,
    })
    .click()
  await expect(composer.getByRole('heading', { name: 'Browser films movies' })).toHaveCount(0)
  expect((await users()).find((row) => row.username === 'viewer')!.libraries).toEqual([])
  expect(
    await (await request.get('/admin/v1/catalogue/collections', { headers })).json(),
  ).toHaveLength(collections.length)
  expect(errors).toEqual([])
})

test('home uses saved progress and advances up next through stable episode links', async ({
  page,
  request,
}) => {
  const headers = { Authorization: `Bearer ${process.env.KAHAWAI_MEDIADB_UI_TOKEN}` }
  const libraries = await (await request.get('/api/v1/catalogue/libraries', { headers })).json()
  const library = libraries.find((l: { name: string }) => l.name === 'Fixture series').id
  const parent = (
    await (await request.get(`/api/v1/catalogue/libraries/${library}/items`, { headers })).json()
  ).items[0].id
  const children = (
    await (
      await request.get(`/api/v1/catalogue/libraries/${library}/items/${parent}/children`, {
        headers,
      })
    ).json()
  ).children
  const mark = async (id: string, played: boolean) => {
    const response = await request.put(
      `/api/v1/catalogue/libraries/${library}/items/${id}/watched`,
      { headers, data: { played } },
    )
    expect(response.ok()).toBe(true)
  }
  await mark(children[0].id, true)
  await page.goto('/app/')
  await page.getByLabel('Username', { exact: true }).fill('fixture')
  await page.getByLabel('Password', { exact: true }).fill('fixture-password')
  await page.getByRole('button', { name: 'Sign in', exact: true }).click()
  await expect(page.getByRole('heading', { name: 'Continue watching', exact: true })).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Up next', exact: true })).toHaveCount(0)
  await page.screenshot({ path: '/tmp/kahawai-continue-watching.png', fullPage: true })
  await page.getByRole('button', { name: /Episode 2/ }).click()
  await expect(page).toHaveURL(
    new RegExp(encodeURIComponent(children[1].id).replaceAll('%3A', '(?:%3A|:)') + '$'),
  )
  await expect(page.getByRole('button', { name: /^(?:▶ )?Resume$/ })).toBeEnabled()
  await mark(children[1].id, false)
  await page.goto('/app/')
  await expect(page.getByRole('heading', { name: 'Up next', exact: true })).toBeVisible()
  await expect(page.getByRole('heading', { name: 'Continue watching', exact: true })).toHaveCount(0)
  await page.screenshot({ path: '/tmp/kahawai-up-next.png', fullPage: true })
  await page.getByRole('button', { name: /Episode 2/ }).click()
  await page.getByRole('button', { name: 'Mark watched', exact: true }).click()
  await expect(page.getByRole('button', { name: 'Watched', exact: true })).toBeVisible()
  await page.goto('/app/')
  await expect(page.getByRole('button', { name: /Episode 3/ })).toBeVisible()
  await mark(children[0].id, false)
  await mark(children[1].id, false)
})

test('physical episodes and tracks keep stable links and source details', async ({
  page,
  request,
}) => {
  const headers = { Authorization: `Bearer ${process.env.KAHAWAI_MEDIADB_UI_TOKEN}` }
  const libraries = (await (
    await request.get('/api/v1/catalogue/libraries', { headers })
  ).json()) as { id: string; name: string }[]
  const shows = libraries.find((l) => l.name === 'Fixture series')!
  const music = libraries.find((l) => l.name === 'Fixture music')!
  const parent = async (library: string) => {
    const result = await request.get(`/api/v1/catalogue/libraries/${library}/items`, { headers })
    expect(result.ok()).toBe(true)
    return (await result.json()).items[0].id as string
  }
  const series = await parent(shows.id)
  const album = await parent(music.id)
  const children = async (library: string, parent: string) => {
    const result = await request.get(
      `/api/v1/catalogue/libraries/${library}/items/${parent}/children`,
      { headers },
    )
    expect(result.ok()).toBe(true)
    return (await result.json()).children as {
      id: string
      title: string
      position: { episode?: number }
    }[]
  }
  const episodes = await children(shows.id, series)
  expect(episodes).toHaveLength(3)
  const second = episodes.find((e) => e.position.episode === 2)!
  await page.goto(`/app/library/${shows.id}/item/${series}`)
  await page.getByLabel('Username', { exact: true }).fill('fixture')
  await page.getByLabel('Password', { exact: true }).fill('fixture-password')
  await page.getByRole('button', { name: 'Sign in', exact: true }).click()
  await expect(page.getByRole('heading', { name: 'Example (2001)', exact: true })).toBeVisible()
  await page
    .getByRole('button', { name: /S01E02/ })
    .first()
    .click()
  await expect(page).toHaveURL(
    new RegExp(encodeURIComponent(second.id).replaceAll('%3A', '(?:%3A|:)') + '$'),
  )
  await page.reload()
  await expect(
    page.getByText('Example (2001)/Example.S01E01-E02.mkv', { exact: true }),
  ).toBeVisible()
  await expect(page.getByRole('button', { name: /^(?:▶ )?Play$/ })).toBeEnabled()
  await page.getByRole('button', { name: '← Example', exact: true }).click()
  await page
    .getByRole('button', { name: /Season 1/ })
    .first()
    .click()
  await expect(page.getByText('3 episodes · 0 watched', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Mark all watched', exact: true }).click()
  await expect(page.getByText('3 episodes · 3 watched', { exact: true })).toBeVisible()
  await page.reload()
  await expect(page.getByText('3 episodes · 3 watched', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Mark none watched', exact: true }).click()
  await expect(page.getByText('3 episodes · 0 watched', { exact: true })).toBeVisible()
  await page.screenshot({ path: '/tmp/kahawai-stable-season.png', fullPage: true })
  const tracks = await children(music.id, album)
  expect(tracks).toHaveLength(2)
  await page.goto(`/app/library/${music.id}/item/${album}`)
  await expect(page.getByText('First song', { exact: true })).toBeVisible()
  await expect(page.getByText('Second song', { exact: true })).toBeVisible()
  await page.screenshot({ path: '/tmp/kahawai-stable-album.png', fullPage: true })
  await page.getByRole('button', { name: /^(?:▶ )?Play$/ }).click()
  await expect
    .poll(
      () =>
        page
          .locator('audio')
          .evaluateAll((elements) =>
            elements.some((element) => (element as HTMLAudioElement).currentTime > 0.2),
          ),
      { timeout: 30_000 },
    )
    .toBe(true)
  await page.getByRole('button', { name: 'Stop and clear the queue', exact: true }).click()
  await page.goto(`/app/library/${music.id}/item/${tracks[0]!.id}`)
  await expect(page.getByRole('heading', { name: 'First song', exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Mark watched', exact: true }).click()
  await expect(page.getByRole('button', { name: 'Watched', exact: true })).toBeVisible()
  await page.reload()
  await expect(page.getByText('01 - First song.flac', { exact: false })).toBeVisible()
  await expect(page.getByRole('button', { name: 'Watched', exact: true })).toBeVisible()
  expect((await children(shows.id, series)).map((c) => c.id)).toEqual(episodes.map((c) => c.id))
})

test('play a catalogue rendition in the existing player and save its progress', async ({
  page,
  request,
}) => {
  const headers = { Authorization: `Bearer ${process.env.KAHAWAI_MEDIADB_UI_TOKEN}` }
  const collections = await (
    await request.get('/admin/v1/catalogue/collections', { headers })
  ).json()
  const collection = collections.find((c: { remote_id: string }) => c.remote_id === 'movies')
  const library = await (
    await request.post('/admin/v1/catalogue/libraries', {
      headers,
      data: { name: 'Playback fixture', media_type: 'movies', collection_ids: [collection.id] },
    })
  ).json()
  const item = (
    await (await request.get(`/api/v1/catalogue/libraries/${library.id}/items`, { headers })).json()
  ).items[0]
  const retired: string[] = []
  page.on('request', (request) => {
    if (/\/api\/v1\/items(?:[/?]|$)/.test(request.url())) retired.push(request.url())
  })
  let sessionId = ''
  page.on('response', async (response) => {
    if (response.url().endsWith('/api/v1/playback/sessions') && response.status() === 201)
      sessionId = (await response.json()).session_id
  })
  await page.goto(`/app/library/${library.id}/item/${item.id}`)
  await page.getByLabel('Username', { exact: true }).fill('fixture')
  await page.getByLabel('Password', { exact: true }).fill('fixture-password')
  await page.getByRole('button', { name: 'Sign in', exact: true }).click()
  await page.getByRole('button', { name: /^(?:▶ )?Play$/ }).click()
  await expect(page).toHaveURL(/\/play(?:\?|$)/)
  const video = page.locator('video').first()
  await expect
    .poll(() => video.evaluate((v: HTMLVideoElement) => v.currentTime), { timeout: 45000 })
    .toBeGreaterThan(1)
  await expect.poll(() => sessionId).not.toBe('')
  await page.getByRole('button', { name: 'Skip intro', exact: true }).click()
  await expect
    .poll(() => video.evaluate((v: HTMLVideoElement) => v.currentTime), { timeout: 30000 })
    .toBeGreaterThanOrEqual(30)
  const subtitles = await request.get(`/api/v1/playback/sessions/${sessionId}/subtitles/1.vtt`, {
    headers,
  })
  expect(subtitles.ok()).toBe(true)
  expect(await subtitles.text()).toContain('Catalogue playback subtitle')
  const seek = await request.post(`/api/v1/playback/sessions/${sessionId}/seek`, {
    headers,
    data: { position_ms: 60000 },
  })
  expect(seek.ok()).toBe(true)
  const saved = await request.post(`/api/v1/playback/sessions/${sessionId}/progress`, {
    headers,
    data: { position_ms: 70000 },
  })
  expect(saved.ok()).toBe(true)
  const feeds = await (await request.get('/api/v1/catalogue/continue-watching', { headers })).json()
  expect(feeds.items.some((row: { id: string }) => row.id === item.id)).toBe(true)
  await page.screenshot({ path: '/tmp/kahawai-mediadb-playback.png', fullPage: true })
  await page.goto('/app/')
  expect(retired).toEqual([])
  await request.delete(`/admin/v1/catalogue/libraries/${library.id}`, { headers })
})

test('anime movies retain Play while anime series expose episodes', async ({ page, request }) => {
  const headers = { Authorization: `Bearer ${process.env.KAHAWAI_MEDIADB_UI_TOKEN}` }
  const libraries = await (await request.get('/api/v1/catalogue/libraries', { headers })).json()
  const library = libraries.find((l: { name: string }) => l.name === 'Fixture anime')
  const path = `/api/v1/catalogue/libraries/${library.id}/items`
  const { items } = await (await request.get(path, { headers })).json()
  const movie = items.find((i: { kind: string }) => i.kind === 'movie')
  const series = items.find((i: { kind: string }) => i.kind === 'series')
  await page.goto(`/app/library/${library.id}/item/${movie.id}`)
  await page.getByLabel('Username', { exact: true }).fill('fixture')
  await page.getByLabel('Password', { exact: true }).fill('fixture-password')
  await page.getByRole('button', { name: 'Sign in', exact: true }).click()
  await expect(page.getByRole('button', { name: /^(?:▶ )?Play$/ })).toBeEnabled()
  await page.goto(`/app/library/${library.id}/item/${series.id}`)
  await expect(page.getByRole('button', { name: /S01E01/ }).first()).toBeVisible()
})

test('community skip lookup runs in the browser with catalogue provider IDs', async ({
  page,
  request,
}) => {
  const headers = { Authorization: `Bearer ${process.env.KAHAWAI_MEDIADB_UI_TOKEN}` }
  const libraries = await (await request.get('/api/v1/catalogue/libraries', { headers })).json()
  const library = libraries.find((l: { name: string }) => l.name === 'Fixture anime')
  const { items } = await (
    await request.get(`/api/v1/catalogue/libraries/${library.id}/items`, { headers })
  ).json()
  const movie = items.find((i: { kind: string }) => i.kind === 'movie')
  expect(
    (
      await request.put('/api/v1/prefs', {
        headers,
        data: { scope: '', key: 'introdb', value: '1' },
      })
    ).ok(),
  ).toBe(true)
  let lookup = ''
  await page.route('https://api.theintrodb.org/**', async (route) => {
    lookup = route.request().url()
    expect(route.request().headers().authorization).toBeUndefined()
    await route.fulfill({ json: { intro: [{ start_ms: 0, end_ms: 30000 }] } })
  })
  await page.goto(`/app/library/${library.id}/item/${movie.id}`)
  await page.getByLabel('Username', { exact: true }).fill('fixture')
  await page.getByLabel('Password', { exact: true }).fill('fixture-password')
  await page.getByRole('button', { name: 'Sign in', exact: true }).click()
  await page.getByRole('button', { name: /^(?:▶ )?Play$/ }).click()
  await expect.poll(() => lookup).toContain('tmdb_id=1234')
  await expect
    .poll(
      () =>
        page
          .locator('video')
          .first()
          .evaluate((v: HTMLVideoElement) => v.currentTime),
      { timeout: 30000 },
    )
    .toBeGreaterThan(1)
  await page.getByRole('button', { name: 'Skip intro', exact: true }).click()
  await expect
    .poll(
      () =>
        page
          .locator('video')
          .first()
          .evaluate((v: HTMLVideoElement) => v.currentTime),
      { timeout: 30000 },
    )
    .toBeGreaterThanOrEqual(30)
  await page.goto('/app/')
  expect(
    (
      await request.put('/api/v1/prefs', {
        headers,
        data: { scope: '', key: 'introdb', value: '0' },
      })
    ).ok(),
  ).toBe(true)
})

test('rendered ASS overlays use the selected catalogue source and session URL', async ({
  page,
  request,
}) => {
  const headers = { Authorization: `Bearer ${process.env.KAHAWAI_MEDIADB_UI_TOKEN}` }
  const collections = await (
    await request.get('/admin/v1/catalogue/collections', { headers })
  ).json()
  const collection = collections.find((c: { remote_id: string }) => c.remote_id === 'movies')
  const library = await (
    await request.post('/admin/v1/catalogue/libraries', {
      headers,
      data: { name: 'Raster fixture', media_type: 'movies', collection_ids: [collection.id] },
    })
  ).json()
  const item = (
    await (await request.get(`/api/v1/catalogue/libraries/${library.id}/items`, { headers })).json()
  ).items[0]
  const path = `/api/v1/catalogue/libraries/${library.id}/items/${item.id}`
  const preview = await (await request.post(path, { headers, data: {} })).json()
  const source = preview.sources.find((s: { streams?: { subtitles: { format: string }[] } }) =>
    s.streams?.subtitles.some((t) => t.format === 'ass'),
  )
  expect(source).toBeTruthy()
  const prefs = (await (await request.get('/api/v1/prefs', { headers })).json()).prefs
  const put = async (key: string, value: string) => {
    expect(
      (await request.put('/api/v1/prefs', { headers, data: { scope: '', key, value } })).ok(),
    ).toBe(true)
  }
  await put('ass_order', 'overlay,flatten,burn')
  await put('subs.movies', 'any')
  await page.addInitScript(() =>
    localStorage.setItem(
      'kahawai.capmask',
      JSON.stringify({ ass_render: false, vtt_render: false }),
    ),
  )
  let session:
    | {
        session_id: string
        media_entry_id: string
        subtitle_listing: { id: number; origin: string; delivery: string }[]
      }
    | undefined
  page.on('response', async (response) => {
    if (response.url().endsWith('/api/v1/playback/sessions') && response.status() === 201)
      session = await response.json()
  })
  const retired: string[] = []
  page.on('request', (r) => {
    if (r.url().includes('/api/v1/items/')) retired.push(r.url())
  })
  try {
    await page.goto(`/app/library/${library.id}/item/${item.id}`)
    await page.getByLabel('Username', { exact: true }).fill('fixture')
    await page.getByLabel('Password', { exact: true }).fill('fixture-password')
    await page.getByRole('button', { name: 'Sign in', exact: true }).click()
    await page.locator('#playback-source').selectOption(String(source.source_id))
    await page.getByRole('button', { name: /^(?:▶ )?(?:Play|Resume)$/ }).click()
    await expect.poll(() => session?.media_entry_id, { timeout: 45000 }).toBe(source.media_entry_id)
    const raster = session!.subtitle_listing.find((t) => t.origin === 'raster')!
    expect(raster.delivery).toBe('overlay')
    const response = await request.get(
      `/api/v1/playback/sessions/${session!.session_id}/subtitles/${raster.id}.jsonl`,
      { headers },
    )
    expect(response.ok()).toBe(true)
    expect((await response.text()).includes('"png"')).toBe(true)
    await expect
      .poll(
        () =>
          page.locator('canvas.imgsub-canvas').evaluate((c: HTMLCanvasElement) => {
            const data = c.getContext('2d')!.getImageData(0, 0, c.width, c.height).data
            return data.some((value, index) => index % 4 === 3 && value > 0)
          }),
        { timeout: 30000 },
      )
      .toBe(true)
    const other = preview.sources.find(
      (s: { media_entry_id: string }) => s.media_entry_id !== source.media_entry_id,
    )
    const otherPreview = await (
      await request.post(path, { headers, data: { media_entry_id: other.media_entry_id } })
    ).json()
    expect(
      otherPreview.negotiated.subtitles.some((t: { origin: string }) => t.origin === 'raster'),
    ).toBe(false)
    expect(retired).toEqual([])
    await page.goto('/app/')
  } finally {
    for (const key of ['ass_order', 'subs.movies'])
      await put(
        key,
        prefs.find((p: { scope: string; key: string }) => p.scope === '' && p.key === key)?.value ??
          '',
      )
    await request.delete(`/admin/v1/catalogue/libraries/${library.id}`, { headers })
  }
})

test('segment administration reports the real mediahost setting in the provider panel', async ({
  page,
}) => {
  await page.goto('/app/admin')
  await page.getByLabel('Username', { exact: true }).fill('fixture')
  await page.getByLabel('Password', { exact: true }).fill('fixture-password')
  await page.getByRole('button', { name: 'Sign in', exact: true }).click()
  await page.getByRole('tab', { name: 'Providers', exact: true }).click()
  const panel = page.getByRole('region', { name: 'Media analysis', exact: true })
  await expect(panel.getByText('detection disabled', { exact: false }).first()).toBeVisible({
    timeout: 45000,
  })
  await expect(
    panel.getByRole('button', { name: 'Find skip points now', exact: true }),
  ).toHaveCount(0)
  await expect(panel).not.toContainText('episodes done since')
  expect(
    (await new AxeBuilder({ page }).include('[aria-labelledby="skip-points"]').analyze())
      .violations,
  ).toEqual([])
  await panel.screenshot({ path: '/tmp/kahawai-segment-admin.png' })
})
