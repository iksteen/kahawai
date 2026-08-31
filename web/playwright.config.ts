import { defineConfig } from '@playwright/test'

const port = 18420

export default defineConfig({
  testDir: './test/browser',
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [['line'], ['html', { open: 'never' }]] : 'line',
  use: {
    baseURL: `http://127.0.0.1:${port}`,
    browserName: 'chromium',
    channel: 'chromium',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  webServer: {
    command: `cargo run --quiet -p kahawai-hub --example csp_browser_fixture -- web/dist 127.0.0.1:${port}`,
    cwd: '..',
    url: `http://127.0.0.1:${port}/app/`,
    reuseExistingServer: false,
    timeout: 120_000,
  },
})
