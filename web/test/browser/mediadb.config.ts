import { defineConfig } from '@playwright/test'

if (!process.env.KAHAWAI_MEDIADB_UI_URL) throw new Error('Run scripts/kahawai-mediadb-ui.sh')

export default defineConfig({
  testDir: '.',
  testMatch: 'mediadb.spec.ts',
  workers: 1,
  retries: 0,
  timeout: 60_000,
  reporter: 'line',
  outputDir: '../../test-results/mediadb',
  use: {
    baseURL: process.env.KAHAWAI_MEDIADB_UI_URL,
    browserName: 'chromium',
    contextOptions: { reducedMotion: 'reduce' },
    channel: 'chrome',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
})
