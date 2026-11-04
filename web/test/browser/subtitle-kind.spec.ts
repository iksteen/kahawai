import { expect, test } from '@playwright/test'
import { execFileSync } from 'node:child_process'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

let work: string
let media: Buffer
test.beforeAll(() => {
  work = mkdtempSync(join(tmpdir(), 'kahawai-subtitle-kind-'))
  const path = join(work, 'fixture.mp4')
  execFileSync('ffmpeg', [
    '-hide_banner',
    '-loglevel',
    'error',
    '-f',
    'lavfi',
    '-i',
    'color=c=blue:s=160x90:r=10:d=30',
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

for (const [order, selected, fallback] of [
  ['forced,sdh,regular', '1', '3'],
  ['regular,forced,sdh', '2', '1'],
  ['sdh,regular,forced', '3', '2'],
]) {
  test(`saved ${order} order controls playback, fallback and manual choices`, async ({
    page,
  }, testInfo) => {
    const errors: string[] = []
    page.on('pageerror', (error) => errors.push(error.message))
    const token = `fixture.${Buffer.from(JSON.stringify({ username: 'fixture', admin: false })).toString('base64url')}.fixture`
    let prefs = [{ scope: '', key: 'subs.movies', value: 'en' }]
    const tracks = [
      { id: 1, forced: true, hearing_impaired: false },
      { id: 2, forced: false, hearing_impaired: false },
      { id: 3, forced: false, hearing_impaired: true },
    ].map((track) => ({
      ...track,
      commentary: false,
      item_id: 'film',
      origin: 'embedded',
      language: 'en',
      format: 'srt',
      delivery: 'text',
      label: null,
      machine: false,
      derived_from: null,
      stream_index: track.id,
      deletable: false,
      note: '',
    }))
    let available = tracks
    const source = {
      source_id: 1,
      collection_item_id: 'physical',
      media_entry_id: 'physical',
      streams: { audio: [], video: [{ codec: 'h264', width: 160, height: 90 }], subtitles: [] },
    }
    let starts = 0
    await page.route('**/api/v1/**', async (route) => {
      const request = route.request()
      const path = new URL(request.url()).pathname
      const json = (value: unknown) => route.fulfill({ json: value })
      if (path.endsWith('/bootstrap'))
        return json({ setup_required: false, setup_available: false })
      if (path.endsWith('/auth/refresh')) return json({ access_token: token, expires_in: 3600 })
      if (path.endsWith('/prefs')) {
        if (request.method() === 'PUT') {
          const pref = request.postDataJSON() as { scope: string; key: string; value: string }
          prefs = prefs.filter((old) => old.scope !== pref.scope || old.key !== pref.key)
          if (pref.value) prefs.push(pref)
          return json({ ok: true })
        }
        return json({ prefs })
      }
      if (path.endsWith('/account/opensubtitles')) return json({ configured: false })
      if (path.endsWith('/libraries'))
        return json([{ id: 'films', name: 'Films', media_type: 'movies', collection_ids: [] }])
      if (path.endsWith('/artwork')) return route.fulfill({ status: 404 })
      if (path.endsWith('/items/film'))
        return json({
          id: 'film',
          title: 'Fixture',
          kind: 'movie',
          year: 2026,
          copy_ids: [],
          representative_id: 'physical',
          played: false,
          metadata: { description: {}, provenance: {}, providers: {} },
          sources: [source],
          copies: [],
          chapters: [],
          segments: [],
          duration_ms: 30_000,
          negotiated: {
            source,
            subtitles: available,
            streams: { video: 'copy', audio: 'off', subtitles: [], cost: 'direct' },
          },
        })
      if (path === '/api/v1/playback/sessions') {
        starts++
        return json({
          session_id: `s${starts}`,
          source_id: 1,
          media_entry_id: 'physical',
          source_fingerprint: 'fixture',
          effective_start_ms: 0,
          stream_url: `/api/v1/playback/sessions/s${starts}/file.mp4`,
          content_type: 'video/mp4',
          mode: 'direct',
          duration_ms: 30_000,
          part_base_ms: 0,
          parts: 1,
          size: media.length,
          subtitle_listing: available,
          streams: { audio: 'off', video: 'copy', subtitles: [], cost: 'direct' },
        })
      }
      if (path.endsWith('/file.mp4'))
        return route.fulfill({ contentType: 'video/mp4', body: media })
      if (path.endsWith('.vtt'))
        return route.fulfill({
          contentType: 'text/vtt',
          body: 'WEBVTT\n\n00:00:00.000 --> 00:00:29.000\nSubtitle fixture\n',
        })
      if (request.method() === 'DELETE') return route.fulfill({ status: 204 })
      return json({})
    })

    await page.goto('/app/settings')
    const list = page.getByRole('list', { name: 'Subtitle types, in order' })
    const positions = ['regular', 'sdh', 'forced']
    for (const [target, kind] of order!.split(',').entries()) {
      let at = positions.indexOf(kind)
      while (at > target) {
        await list.getByRole('listitem').nth(at).press('ArrowUp')
        positions.splice(at - 1, 0, positions.splice(at, 1)[0]!)
        at--
      }
    }
    await expect.poll(() => prefs.find((pref) => pref.key === 'subs.kind')?.value).toBe(order)
    await page.reload()
    const labels = { regular: 'Regular', sdh: 'SDH', forced: 'Forced' }
    for (const [at, kind] of order!.split(',').entries()) {
      await expect(list.getByRole('listitem').nth(at)).toHaveAttribute(
        'aria-label',
        new RegExp(`^${labels[kind as keyof typeof labels]},`),
      )
    }
    await page
      .locator('section')
      .filter({ has: list })
      .screenshot({ path: testInfo.outputPath(`subtitle-${selected}.png`) })

    await page.goto('/app/library/films/item/film/play')
    const picker = page.getByLabel('Subtitles', { exact: true })
    await expect(picker).toHaveValue(selected!)
    await expect(picker.locator('option')).toHaveCount(4)
    available = tracks.filter((track) => String(track.id) !== selected)
    await page.reload()
    await expect(picker).toHaveValue(fallback!)
    available = tracks
    await page.reload()
    await expect(picker).toHaveValue(selected!)
    await page.locator('.videobox').hover()
    const manual = selected === '1' ? '2' : '1'
    await picker.selectOption(manual)
    await expect(picker).toHaveValue(manual)
    await expect
      .poll(
        () =>
          prefs.find((pref) => pref.scope === 'source:physical:1' && pref.key === 'subs.track')
            ?.value,
      )
      .toBe(manual)

    // A manual title choice still wins over the automatic order on reopen.
    await page.reload()
    await expect(picker).toHaveValue(manual)
    // If the remembered track disappears, the ordered preference takes over.
    available = tracks.filter((track) => String(track.id) !== manual)
    await page.reload()
    await expect(picker).toHaveValue(selected!)
    expect(errors).toEqual([])
  })
}
