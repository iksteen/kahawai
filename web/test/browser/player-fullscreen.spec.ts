import { expect, test } from '@playwright/test'
import { execFileSync } from 'node:child_process'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

// The real browser owns fullscreen and media events; only hub replies are
// fixtures. A DOM-only test cannot detect fullscreen exiting on node removal.
let work: string
let media: Buffer
test.beforeAll(() => {
  work = mkdtempSync(join(tmpdir(), 'kahawai-fullscreen-'))
  const path = join(work, 'episode.mp4')
  execFileSync('ffmpeg', [
    '-hide_banner',
    '-loglevel',
    'error',
    '-f',
    'lavfi',
    '-i',
    'color=c=blue:s=160x90:r=10:d=10',
    '-c:v',
    'libx264',
    '-pix_fmt',
    'yuv420p',
    '-movflags',
    '+faststart',
    path,
  ])
  media = readFileSync(path)
})
test.afterAll(() => rmSync(work, { recursive: true, force: true }))

for (const advance of ['ended', 'Play now']) {
  test(`fullscreen survives advancing with ${advance}`, async ({ page }) => {
    const errors: string[] = []
    page.on('pageerror', (error) => errors.push(error.message))
    const token = `fixture.${Buffer.from(JSON.stringify({ username: 'fixture', admin: false })).toString('base64url')}.fixture`
    const metadata = { description: {}, provenance: {}, providers: {} }
    const child = (id: string) => ({
      id,
      title: id,
      parent_id: 'show',
      position: { kind: 'episode', season: 1, episode: id === 'one' ? 1 : 2 },
      representative_id: `copy-${id}`,
      source_count: 1,
      metadata,
    })
    const streams = { audio: [], video: [{ codec: 'h264', width: 160, height: 90 }], subtitles: [] }
    const source = {
      source_id: 1,
      media_entry_id: 'copy',
      streams,
      display_width: 160,
      display_height: 90,
    }
    let starts = 0
    await page.route('**/api/v1/**', async (route) => {
      const request = route.request()
      const path = new URL(request.url()).pathname
      const json = (value: unknown) => route.fulfill({ json: value })
      if (path.endsWith('/bootstrap'))
        return json({ setup_required: false, setup_available: false })
      if (path.endsWith('/auth/refresh')) return json({ access_token: token, expires_in: 3600 })
      if (path.endsWith('/prefs')) return json({ prefs: [] })
      if (path.endsWith('/libraries'))
        return json([{ id: 'shows', name: 'Shows', media_type: 'series', collection_ids: [] }])
      if (path.endsWith('/next')) return json(path.includes('/one/') ? child('two') : null)
      if (path.endsWith('/artwork')) return route.fulfill({ status: 404 })
      if (/\/items\/(one|two)$/.test(path)) {
        const id = path.split('/').at(-1)!
        return json({
          id,
          title: id,
          kind: 'series',
          media_type: 'series',
          child: child(id),
          metadata,
          parent_title: 'Show',
          copy_ids: [],
          representative_id: `copy-${id}`,
          played: false,
          copies: [],
          sources: [source],
          chapters: [],
          segments: [],
          duration_ms: 10_000,
          negotiated: {
            source,
            subtitles: [],
            streams: { audio: 'off', video: 'copy', subtitles: [], cost: 'direct' },
          },
        })
      }
      if (path === '/api/v1/playback/sessions') {
        starts++
        return json({
          session_id: `s${starts}`,
          source_id: 1,
          effective_start_ms: 0,
          stream_url: `/api/v1/playback/sessions/s${starts}/file.mp4`,
          content_type: 'video/mp4',
          mode: 'direct',
          duration_ms: 10_000,
          part_base_ms: 0,
          parts: 1,
          size: media.length,
          subtitle_listing: [],
          streams: { audio: 'off', video: 'copy', subtitles: [], cost: 'direct' },
        })
      }
      if (path.endsWith('/file.mp4'))
        return route.fulfill({ contentType: 'video/mp4', body: media })
      if (request.method() === 'DELETE') return route.fulfill({ status: 204 })
      return json({})
    })
    await page.goto('/app/library/shows/item/one/play')
    const video = page.locator('video')
    await expect(video).toBeVisible()
    const veil = page.locator('button.play-veil')
    if (await veil.isVisible()) await veil.click()
    await expect
      .poll(() => video.evaluate((v) => (v as HTMLVideoElement).currentTime))
      .toBeGreaterThan(0)
    await expect(page.getByRole('region', { name: 'Up next' })).toBeVisible()
    await page.getByRole('button', { name: 'Fullscreen', exact: true }).click()
    await expect.poll(() => page.evaluate(() => document.fullscreenElement !== null)).toBe(true)
    const frame = await page.locator('main.player-frame').elementHandle()
    const oldVideo = await video.elementHandle()
    if (advance === 'Play now') {
      await page.getByRole('button', { name: 'Play now', exact: true }).click()
    }
    // The automatic case plays the clip to completion and fires native ended.
    await expect(page).toHaveURL(/\/item\/two\/play$/, { timeout: 15_000 })
    expect(starts).toBe(2)
    expect(await oldVideo!.evaluate((v) => v.isConnected)).toBe(false)
    expect(await frame!.evaluate((f) => document.fullscreenElement === f)).toBe(true)
    await expect
      .poll(() => video.evaluate((v) => (v as HTMLVideoElement).currentTime))
      .toBeGreaterThan(0)
    expect(
      await video.evaluate((v) => {
        const box = v.getBoundingClientRect()
        return box.width === window.innerWidth && box.height === window.innerHeight
      }),
    ).toBe(true)
    await page.evaluate(() => document.exitFullscreen())
    await expect(page.getByRole('button', { name: 'Fullscreen', exact: true })).toBeVisible()
    expect(errors).toEqual([])
  })
}
