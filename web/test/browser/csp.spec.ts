import { expect, test, type Page } from '@playwright/test'
import { readdirSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const assets = resolve(dirname(fileURLToPath(import.meta.url)), '../../dist/assets')
const jassubChunk = readdirSync(assets).find((name) => /^jassub-[\w-]+\.js$/.test(name))
const jassubFont = readdirSync(assets).find((name) => /^default-[\w-]+\.woff2$/.test(name))

if (!jassubChunk) throw new Error(`production bundle has no JASSUB chunk in ${assets}`)
if (!jassubFont) throw new Error(`production bundle has no JASSUB font in ${assets}`)

type Violation = { directive: string; blocked: string }

async function recordViolations(page: Page) {
  await page.addInitScript(() => {
    const target = window as typeof window & { __kahawaiCspViolations?: Violation[] }
    target.__kahawaiCspViolations = []
    document.addEventListener('securitypolicyviolation', (event) => {
      target.__kahawaiCspViolations?.push({
        directive: event.effectiveDirective,
        blocked: event.blockedURI,
      })
    })
  })
}

async function violations(page: Page): Promise<Violation[]> {
  return page.evaluate(
    () =>
      (window as typeof window & { __kahawaiCspViolations?: Violation[] }).__kahawaiCspViolations ??
      [],
  )
}

test('the production SPA and its required browser capabilities run under CSP', async ({ page }) => {
  await recordViolations(page)
  const stylesheets: string[] = []
  const jassubResources: string[] = []
  page.on('response', (response) => {
    if (response.url().endsWith('.css') && response.ok()) stylesheets.push(response.url())
    if (
      response.ok() &&
      response.url().includes('/app/assets/') &&
      (/worker.*\.js$/.test(response.url()) || /\.(wasm|woff2)$/.test(response.url()))
    ) {
      jassubResources.push(response.url())
    }
  })

  const response = await page.goto('/app/')
  expect(response?.headers()['content-security-policy']).toBe(
    "default-src 'none'; base-uri 'none'; object-src 'none'; frame-src 'none'; frame-ancestors 'none'; form-action 'self'; script-src 'self' 'wasm-unsafe-eval'; script-src-attr 'none'; style-src-elem 'self'; style-src-attr 'unsafe-inline'; img-src 'self' data:; font-src 'self'; media-src 'self' blob:; connect-src 'self' https://api.theintrodb.org; worker-src 'self' blob:;",
  )
  await expect(page.getByLabel('Username')).toBeVisible()
  expect(stylesheets.length).toBeGreaterThan(0)

  await page.evaluate(
    async ({ chunk, font }) => {
      const { default: JASSUB } = await import(`/app/assets/${chunk}`)
      const canvas = document.createElement('canvas')
      canvas.width = 320
      canvas.height = 180
      document.body.append(canvas)
      const renderer = new JASSUB({
        canvas,
        queryFonts: false,
        fonts: [`/app/assets/${font}`],
        subContent:
          '[Script Info]\nScriptType: v4.00+\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Liberation Sans,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,10,1\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:00.00,0:00:02.00,Default,,0,0,0,,SEC-WEB-1\n',
      })
      await renderer.ready
      await renderer.manualRender(
        { expectedDisplayTime: performance.now(), width: 320, height: 180, mediaTime: 0.5 },
        true,
      )
      await renderer.destroy()
    },
    { chunk: jassubChunk, font: jassubFont },
  )

  expect(jassubResources.some((url) => /worker.*\.js$/.test(url))).toBe(true)
  expect(jassubResources.some((url) => url.endsWith('.wasm'))).toBe(true)
  expect(jassubResources.some((url) => url.endsWith('.woff2'))).toBe(true)

  await page.waitForTimeout(100)
  await expect
    .poll(() => violations(page), { message: 'the production app caused a CSP violation' })
    .toEqual([])

  const capabilities = await page.evaluate(async () => {
    const style = document.createElement('div')
    style.style.width = '37px'
    document.body.append(style)

    const image = new Image()
    const imageLoaded = new Promise<boolean>((done) => {
      image.onload = () => done(true)
      image.onerror = () => done(false)
    })
    image.src =
      'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M/wHwAF/gL+X2NDWQAAAABJRU5ErkJggg=='

    const blobWorker = new Worker(
      URL.createObjectURL(
        new Blob(['self.onmessage=()=>self.postMessage("ready")'], { type: 'text/javascript' }),
      ),
    )
    const workerReady = await new Promise<boolean>((done) => {
      blobWorker.onmessage = () => done(true)
      blobWorker.onerror = () => done(false)
      blobWorker.postMessage(null)
    })
    blobWorker.terminate()

    const video = document.createElement('video')
    video.src = URL.createObjectURL(new Blob([], { type: 'video/mp4' }))
    const mediaSettled = new Promise<boolean>((done) => {
      video.onloadedmetadata = () => done(true)
      // An empty MP4 is expected to fail decoding; reaching the media loader
      // without a CSP violation is the capability under test.
      video.onerror = () => done(true)
      setTimeout(() => done(false), 2_000)
    })
    video.load()

    return {
      dynamicStyle: getComputedStyle(style).width,
      dataImage: await imageLoaded,
      blobWorker: workerReady,
      blobMedia: video.src.startsWith('blob:'),
      mediaSettled: await mediaSettled,
    }
  })
  expect(capabilities).toEqual({
    dynamicStyle: '37px',
    dataImage: true,
    blobWorker: true,
    blobMedia: true,
    mediaSettled: true,
  })
  await page.waitForTimeout(100)
  expect(await violations(page)).toEqual([])
})

test('CSP blocks authority the application was not granted', async ({ page }) => {
  await recordViolations(page)
  await page.goto('/app/')
  await expect(page.getByLabel('Username')).toBeVisible()

  const blocked = await page.evaluate(async () => {
    const evalScript = document.createElement('script')
    evalScript.src = '/api/v1/security/eval-script.js'
    await new Promise<void>((done, reject) => {
      evalScript.onload = () => done()
      evalScript.onerror = () => reject(new Error('the allowed same-origin script did not load'))
      document.head.append(evalScript)
    })

    const inline = document.createElement('script')
    inline.textContent = 'globalThis.__kahawaiInlineExecuted = true'
    document.head.append(inline)

    let connected = true
    try {
      await fetch('https://example.invalid/sec-web-1')
    } catch {
      connected = false
    }
    return {
      evaluated: Boolean(
        (globalThis as typeof globalThis & { __kahawaiEvalExecuted?: boolean })
          .__kahawaiEvalExecuted,
      ),
      evalBlocked: Boolean(
        (globalThis as typeof globalThis & { __kahawaiEvalBlocked?: boolean }).__kahawaiEvalBlocked,
      ),
      inline: Boolean(
        (globalThis as typeof globalThis & { __kahawaiInlineExecuted?: boolean })
          .__kahawaiInlineExecuted,
      ),
      connected,
    }
  })
  expect(blocked).toEqual({
    evaluated: false,
    evalBlocked: true,
    inline: false,
    connected: false,
  })
  await expect
    .poll(() => violations(page))
    .toEqual(
      expect.arrayContaining([
        expect.objectContaining({ directive: 'script-src', blocked: 'eval' }),
        expect.objectContaining({ directive: 'script-src-elem', blocked: 'inline' }),
        expect.objectContaining({ directive: 'connect-src' }),
      ]),
    )
})

test('framing and MIME-sniffed script execution are refused', async ({ page }) => {
  await page.goto('/security/frame-probe')
  await expect(page.locator('body')).toHaveAttribute('data-probe-loaded', 'yes')
  expect(
    await page
      .locator('#probe')
      .evaluate((frame: HTMLIFrameElement) =>
        Boolean(frame.contentDocument?.querySelector('#app')),
      ),
  ).toBe(false)

  await page.goto('/app/')
  await expect(page.getByLabel('Username')).toBeVisible()
  const scriptBlocked = await page.evaluate(async () => {
    const script = document.createElement('script')
    script.src = '/api/v1/security/nosniff-script'
    const result = new Promise<boolean>((done) => {
      script.onload = () => done(false)
      script.onerror = () => done(true)
    })
    document.head.append(script)
    return result
  })
  expect(scriptBlocked).toBe(true)
  expect(
    await page.evaluate(() =>
      Boolean(
        (globalThis as typeof globalThis & { __kahawaiNosniffExecuted?: boolean })
          .__kahawaiNosniffExecuted,
      ),
    ),
  ).toBe(false)
})

test('the minimal permissions policy is delivered and understood by Chromium', async ({ page }) => {
  const response = await page.goto('/app/')
  expect(response?.headers()['permissions-policy']).toBe(
    'accelerometer=(), autoplay=(self), camera=(), clipboard-read=(), clipboard-write=(self), display-capture=(), encrypted-media=(), fullscreen=(self), geolocation=(), gyroscope=(), hid=(), local-fonts=(), magnetometer=(), microphone=(), midi=(), payment=(), picture-in-picture=(self), screen-wake-lock=(), serial=(), usb=(), xr-spatial-tracking=()',
  )
  const policy = await page.evaluate(() => {
    const documentPolicy =
      (
        document as typeof document & {
          permissionsPolicy?: { allowsFeature(name: string): boolean }
          featurePolicy?: { allowsFeature(name: string): boolean }
        }
      ).permissionsPolicy ??
      (document as typeof document & { featurePolicy?: { allowsFeature(name: string): boolean } })
        .featurePolicy
    if (!documentPolicy) throw new Error('Chromium exposes no Permissions Policy API')
    return {
      autoplay: documentPolicy.allowsFeature('autoplay'),
      fullscreen: documentPolicy.allowsFeature('fullscreen'),
      camera: documentPolicy.allowsFeature('camera'),
      microphone: documentPolicy.allowsFeature('microphone'),
      geolocation: documentPolicy.allowsFeature('geolocation'),
    }
  })
  expect(policy).toEqual({
    autoplay: true,
    fullscreen: true,
    camera: false,
    microphone: false,
    geolocation: false,
  })
})
