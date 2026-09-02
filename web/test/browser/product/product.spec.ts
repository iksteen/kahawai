import AxeBuilder from '@axe-core/playwright'
import { expect, test, type BrowserContext, type Locator, type Page } from '@playwright/test'

import { CONTROL, PUBLIC, SETUP } from './addresses.ts'
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

function disableMediaSource() {
  // hls.js can use all three names. Current Safari exposes
  // ManagedMediaSource as well as MediaSource, so masking only the latter
  // still exercises its blob/MSE path instead of native HLS.
  for (const name of ['MediaSource', 'ManagedMediaSource', 'WebKitMediaSource']) {
    Object.defineProperty(globalThis, name, {
      configurable: true,
      value: undefined,
    })
  }
}

async function brightPixels(target: Page, canvas: Locator): Promise<number> {
  const png = await canvas.screenshot()
  return target.evaluate(async (encoded) => {
    const image = new Image()
    image.src = `data:image/png;base64,${encoded}`
    await image.decode()
    const sample = document.createElement('canvas')
    sample.width = image.naturalWidth
    sample.height = image.naturalHeight
    const context = sample.getContext('2d')
    if (!context) return 0
    context.drawImage(image, 0, 0)
    const pixels = context.getImageData(0, 0, sample.width, sample.height).data
    let bright = 0
    for (let at = 0; at < pixels.length; at += 4) {
      if (pixels[at]! + pixels[at + 1]! + pixels[at + 2]! > 90) bright++
    }
    return bright
  }, png.toString('base64'))
}

async function accessible(name: string) {
  if (engine !== 'chromium') return
  const result = await new AxeBuilder({ page })
    .withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'])
    .analyze()
  if (result.violations.length) {
    await test.info().attach(`axe-${name}.json`, {
      body: JSON.stringify(result, null, 2),
      contentType: 'application/json',
    })
  }
  expect(
    result.violations.map((violation) => ({
      id: violation.id,
      impact: violation.impact,
      targets: violation.nodes.map((node) => node.target),
    })),
    `${name} has automated accessibility violations`,
  ).toEqual([])
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
  await target.getByRole('button', { name: /^▶ (?:Play|Resume)$/ }).click()
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
    // Chromium owns the hls.js/MediaSource recovery gate. Playwright drives
    // WebKit through Web Inspector, whose inspected MediaSource process can
    // stop responding after a successful append on hosted macOS runners.
    // Safari's production path is native HLS, so exercise every WebKit
    // playback mode through that path from the first page load.
    if (engine === 'webkit' && process.platform === 'darwin') {
      await context.addInitScript(disableMediaSource)
    }
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

  test.afterEach(() => {
    expect(pageErrors, 'the product emitted an uncaught page error').toEqual([])
    expect(foreignRequests, 'the hermetic product journey attempted a foreign request').toEqual([])
  })

  test('setup is local-only and hands the installation to normal login', async () => {
    await page.goto(`${PUBLIC}/app/`)
    await expect(page.getByText('Initial setup is available only')).toBeVisible()

    await page.goto(`${SETUP}/app/`)
    await expect(page.getByText('First run. Create the initial administrator')).toBeVisible()
    await accessible('setup')
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
    await accessible('login')
    await page.getByLabel('Username').fill('admin')
    await page.getByLabel('Password').fill(ADMIN_PASSWORD)
    await page.getByRole('button', { name: 'Sign in' }).click()
    await expect(page.getByRole('heading', { name: 'Your libraries' })).toBeVisible()
    await accessible('browse-home')
  })

  test('administration creates a composed library and a narrowed account', async () => {
    await page.goto(`${PUBLIC}/app/admin`)
    await expect(page.getByRole('heading', { name: 'Satellites' })).toBeVisible()
    await expect(page.getByText('No satellites enrolled.')).toBeVisible()
    await accessible('admin-satellites')

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
    await accessible('admin-libraries')

    await page.getByRole('tab', { name: 'Providers' }).click()
    await expect(page.getByRole('heading', { name: 'Matching order' })).toBeVisible()
    await accessible('admin-providers')
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
    await accessible('admin-users')

    await page.getByRole('tab', { name: 'Sessions' }).click()
    await expect(page.getByText('Nobody is streaming.')).toBeVisible()
    await accessible('admin-sessions')
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
    await accessible('detail')
  })

  test('direct, remux and transcode modes decode and recover in the browser', async () => {
    let video = await startPlayback('DIRECT')
    expect(await video.evaluate((element) => (element as HTMLVideoElement).currentSrc)).not.toMatch(
      /^blob:/,
    )
    // This fixture is intentionally tiny. On a slower runner it can finish
    // between startPlayback's assertions and this cleanup, restoring the play
    // veil and making a pointer click on the video impossible. Pause the media
    // element directly; navigation below still owns session teardown.
    await video.evaluate((element) => (element as HTMLVideoElement).pause())
    await page.getByRole('button', { name: '← Back' }).click()

    // WebKit implements route.abort() through Web Inspector. It does retry the
    // blocked segment, but its inspected MediaSource can remain wedged at time
    // zero after the successful retry. Chromium exercises the real hls.js
    // transient-network recovery; WebKit gates every playback mode through
    // native HLS and makes that source path explicit below.
    let failedSegment = false
    if (engine !== 'webkit') {
      await page.route('**/segment*.ts', async (route) => {
        if (!failedSegment) {
          failedSegment = true
          await route.abort('connectionreset')
        } else {
          await route.continue()
        }
      })
    }
    await openFixture('Remux Fixture')
    video = await startPlayback('REMUX')
    if (engine !== 'webkit') expect(failedSegment).toBe(true)
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
    if (engine !== 'webkit') await page.unroute('**/segment*.ts')
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
    const video = await startPlayback('REMUX')
    const subtitles = page.getByLabel('Subtitles')
    await subtitles.selectOption({ index: 1 })
    const canvas = page.locator('.videobox canvas')
    await expect(canvas).toBeVisible()
    await expect
      .poll(() => canvas.evaluate((element) => [element.clientWidth, element.clientHeight]))
      .not.toEqual([0, 0])
    await video.evaluate(async (element) => {
      const player = element as HTMLVideoElement
      player.playbackRate = 0.1
      player.currentTime = 0.5
      await player.play()
    })
    await expect.poll(() => brightPixels(page, canvas)).toBeGreaterThan(0)
    await accessible('player')
    // Tear the worker and its streaming fetch down while this test still owns
    // the page. WebKit otherwise reports the fetch aborted by the next test's
    // navigation as a late page error in that unrelated test.
    await subtitles.selectOption('')
    await expect(canvas).toBeHidden()
  })

  test('settings and all primary screens pass the automated accessibility gate', async () => {
    await page.goto(`${PUBLIC}/app/settings`)
    await expect(page.getByRole('heading', { name: 'Settings' })).toBeVisible()
    await accessible('settings')
  })

  test('WebKit consumes HLS natively when MediaSource is unavailable', async () => {
    test.skip(engine !== 'webkit' || process.platform !== 'darwin', 'native HLS is the macOS gate')
    const native = await context.newPage()
    watch(native)
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
