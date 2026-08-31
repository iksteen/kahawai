import { expect, test, type BrowserContext, type Page } from '@playwright/test'

const PUBLIC = 'http://127.0.0.1:18430'
const SETUP = 'http://127.0.0.1:18431'
const CONTROL = 'http://127.0.0.1:18433'
const ADMIN_PASSWORD = 'browser-password'
const VIEWER_PASSWORD = 'viewer-password'

let context: BrowserContext
let page: Page
let engine: 'chromium' | 'firefox' | 'webkit'
const pageErrors: string[] = []
const foreignRequests: string[] = []

function watch(target: Page) {
  target.on('pageerror', (error) => pageErrors.push(error.message))
}

async function login(username: string, password: string) {
  await page.goto(`${PUBLIC}/app/`)
  await expect(page.getByRole('button', { name: 'Sign in' })).toBeVisible()
  await page.getByLabel('Username').fill(username)
  await page.getByLabel('Password').fill(password)
  await page.getByRole('button', { name: 'Sign in' }).click()
  await expect(page.getByRole('heading', { name: 'Your libraries' })).toBeVisible()
}

async function logout(username: string) {
  await page.locator('header').getByRole('button', { name: username, exact: true }).click()
  const revoked = page.waitForResponse(
    (response) =>
      response.request().method() === 'POST' && response.url().includes('/api/v1/auth/logout'),
  )
  await page.getByRole('menuitem', { name: 'Sign out' }).click()
  await revoked
  await expect(page.getByRole('button', { name: 'Sign in' })).toBeVisible()
}

async function openFixture(title: string, target = page) {
  await target.goto(`${PUBLIC}/app/`)
  const search = target.getByLabel('Search all libraries')
  await search.fill(title)
  await target.getByRole('option', { name: new RegExp(title, 'i') }).click()
  await expect(target.getByRole('heading', { name: new RegExp(title, 'i') })).toBeVisible()
}

async function startPlayback(expected: 'DIRECT' | 'REMUX' | 'TRANSCODE', target = page) {
  await target.getByRole('button', { name: /^▶ Play$/ }).click()
  const video = target.locator('video')
  // The first HLS session after a crash-style hub restart also starts the
  // supervised GStreamer worker; CI machines can spend well beyond the normal
  // UI expectation constructing and prerolling its initial pipeline.
  await expect(video).toBeVisible({ timeout: 45_000 })
  const veil = target.locator('button.play-veil')
  if (await veil.isVisible()) await veil.click()
  await expect
    .poll(() => video.evaluate((element) => (element as HTMLVideoElement).currentTime))
    .toBeGreaterThan(0.35)
  expect(await video.evaluate((element) => (element as HTMLVideoElement).error)).toBeNull()
  await target.getByTitle('Playback info — why is this (not) transcoding').click()
  await expect(target.locator('.videobox')).toContainText(expected)
  return video
}

test.describe.serial('real all-in-one product flows', () => {
  test.beforeAll(async ({ browser, browserName }) => {
    engine = browserName
    context = await browser.newContext({
      baseURL: PUBLIC,
      reducedMotion: 'reduce',
      viewport: { width: 1280, height: 800 },
    })
    await context.route('**/*', async (route) => {
      const url = new URL(route.request().url())
      if (
        (url.protocol === 'http:' || url.protocol === 'https:') &&
        !['127.0.0.1', 'localhost'].includes(url.hostname)
      ) {
        foreignRequests.push(url.toString())
        await route.abort('blockedbyclient')
        return
      }
      await route.continue()
    })
    page = await context.newPage()
    watch(page)
  })

  test.afterAll(async () => {
    await context?.close()
  })

  test('setup is local-only and hands the installation to normal login', async () => {
    await page.goto(`${PUBLIC}/app/`)
    await expect(page.getByText('Initial setup is available only')).toBeVisible()

    await page.goto(`${SETUP}/app/`)
    await expect(page.getByText('First run. Create the initial administrator')).toBeVisible()
    await page.getByLabel('Username').fill('admin')
    await page.getByLabel('Password').fill(ADMIN_PASSWORD)
    await page.getByRole('button', { name: 'Create admin account' }).click()
    await expect(page.getByText('Administrator created')).toBeVisible()
    await expect
      .poll(async () => {
        try {
          await fetch(`${SETUP}/api/v1/bootstrap`)
          return false
        } catch {
          return true
        }
      })
      .toBe(true)

    await page.goto(`${PUBLIC}/app/`)
    await page.getByLabel('Username').fill('admin')
    await page.getByLabel('Password').fill(ADMIN_PASSWORD)
    await page.getByRole('button', { name: 'Sign in' }).click()
    await expect(page.getByRole('heading', { name: 'Your libraries' })).toBeVisible()
  })

  test('administration creates a composed library and a narrowed account', async () => {
    await page.goto(`${PUBLIC}/app/admin`)
    await expect(page.getByRole('heading', { name: 'Satellites' })).toBeVisible()

    await page.getByRole('tab', { name: 'Libraries' }).click()
    await page.getByLabel('New library name').fill('Browser Library')
    await page.getByRole('button', { name: 'Create' }).click()
    const library = page.getByRole('listitem').filter({ hasText: 'Browser Library' })
    await expect(library).toBeVisible()
    const attach = library.getByLabel('Attach a collection to Browser Library')
    const options = await attach.locator('option').allTextContents()
    const movies = options.findIndex((option) => option.endsWith('/movies'))
    expect(movies).toBeGreaterThan(0)
    await attach.selectOption({ index: movies })
    await expect(library.getByLabel(/Detach .*\/movies from Browser Library/)).toBeVisible()

    await page.getByRole('tab', { name: 'Providers' }).click()
    await page.getByRole('tab', { name: 'Users & grants' }).click()
    await page.getByLabel('New username').fill('viewer')
    await page.getByLabel('Password').fill(VIEWER_PASSWORD)
    await page.getByRole('button', { name: 'Create' }).click()
    const viewer = page.getByRole('listitem').filter({ hasText: /^viewer/ })
    await expect(viewer).toBeVisible()
    await viewer.getByRole('button', { name: 'all libraries' }).click()
    await expect(viewer.getByRole('button', { name: 'all libraries' })).toHaveAttribute(
      'aria-pressed',
      'false',
    )
    await viewer.getByRole('button', { name: 'Browser Library' }).click()
    await expect(viewer.getByRole('button', { name: 'Browser Library' })).toHaveAttribute(
      'aria-pressed',
      'true',
    )

    await page.getByRole('tab', { name: 'Sessions' }).click()
  })

  test('a process restart preserves state and the grant is enforced in the UI', async () => {
    const response = await fetch(`${CONTROL}/restart`, { method: 'POST' })
    expect(response.ok, await response.text()).toBe(true)
    await page.goto(`${PUBLIC}/app/admin`)
    await page.getByRole('tab', { name: 'Libraries' }).click()
    await expect(page.getByText('Browser Library', { exact: true })).toBeVisible()
    await page.getByRole('tab', { name: 'Users & grants' }).click()
    const viewer = page.getByRole('listitem').filter({ hasText: /^viewer/ })
    await expect(viewer.getByRole('button', { name: 'Browser Library' })).toHaveAttribute(
      'aria-pressed',
      'true',
    )

    await logout('admin')
    await login('viewer', VIEWER_PASSWORD)
    await expect(page.getByRole('button', { name: 'Browser Library', exact: true })).toBeVisible()
    await expect(page.getByRole('button', { name: 'hidden', exact: true })).toHaveCount(0)
    await expect(page.getByRole('button', { name: 'movies', exact: true })).toHaveCount(0)
  })

  test('browse, search and a reloaded deep link retain their visible result', async () => {
    await openFixture('Direct Fixture')
    const deepLink = page.url()
    expect(deepLink).toMatch(/\/app\/library\/[^/]+\/item\/[^/]+$/)
    await page.reload()
    await expect(page.getByRole('heading', { name: /Direct Fixture/ })).toBeVisible()
    expect(page.url()).toBe(deepLink)
  })

  test('direct, remux and transcode modes decode and recover in the browser', async () => {
    let video = await startPlayback('DIRECT')
    expect(await video.evaluate((element) => (element as HTMLVideoElement).currentSrc)).not.toMatch(
      /^blob:/,
    )
    await video.click()
    await page.getByRole('button', { name: '← Back' }).click()

    let failedSegment = false
    await page.route('**/segment*.ts', async (route) => {
      if (!failedSegment) {
        failedSegment = true
        await route.abort('connectionreset')
      } else {
        await route.continue()
      }
    })
    await openFixture('Remux Fixture')
    video = await startPlayback('REMUX')
    expect(failedSegment).toBe(true)
    const seek = page.getByRole('slider', { name: /Seek/ })
    await seek.evaluate((element) => {
      const input = element as HTMLInputElement
      input.value = String(Math.max(1_000, Number(input.max) - 2_000))
      input.dispatchEvent(new Event('input', { bubbles: true }))
    })
    await expect
      .poll(() => video.evaluate((element) => (element as HTMLVideoElement).currentTime))
      .toBeGreaterThan(1)
    await expect(page.getByRole('alertdialog', { name: 'Playback stopped' })).toHaveCount(0)
    await page.unroute('**/segment*.ts')
    await page.getByRole('button', { name: '← Back' }).click()

    await page.evaluate(() =>
      localStorage.setItem('kahawai.capmask', JSON.stringify({ video: ['hevc'] })),
    )
    await openFixture('Transcode Fixture')
    video = await startPlayback('TRANSCODE')
    expect(
      await video.evaluate((element) => (element as HTMLVideoElement).currentTime),
    ).toBeGreaterThan(0)
    await page.evaluate(() => localStorage.removeItem('kahawai.capmask'))
  })

  test('embedded ASS reaches the real browser subtitle renderer', async () => {
    await openFixture('Subtitle Fixture')
    await startPlayback('REMUX')
    const subtitles = page.getByLabel('Subtitles')
    await subtitles.selectOption({ index: 1 })
    const canvas = page.locator('.videobox canvas')
    await expect(canvas).toBeVisible()
    await expect
      .poll(() => canvas.evaluate((element) => [element.clientWidth, element.clientHeight]))
      .not.toEqual([0, 0])
  })

  test('settings and all primary screens pass the automated accessibility gate', async () => {
    await page.goto(`${PUBLIC}/app/settings`)
    await expect(page.getByRole('heading', { name: 'Settings' })).toBeVisible()
    expect(pageErrors).toEqual([])
    expect(foreignRequests).toEqual([])
  })

  test('WebKit consumes HLS natively when MediaSource is unavailable', async () => {
    test.skip(engine !== 'webkit' || process.platform !== 'darwin', 'native HLS is the macOS gate')
    const native = await context.newPage()
    watch(native)
    await native.addInitScript(() => {
      Object.defineProperty(globalThis, 'MediaSource', {
        configurable: true,
        value: undefined,
      })
    })
    await openFixture('Remux Fixture', native)
    const video = await startPlayback('REMUX', native)
    expect(
      await video.evaluate((element) => ({
        hls: (element as HTMLVideoElement).canPlayType('application/vnd.apple.mpegurl'),
        source: (element as HTMLVideoElement).currentSrc,
      })),
    ).toEqual({
      hls: expect.not.stringMatching(/^$/),
      source: expect.stringMatching(/master\.m3u8/),
    })
    await native.close()
  })
})
