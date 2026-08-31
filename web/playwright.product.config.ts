import { defineConfig, devices } from '@playwright/test'

const publicPort = Number(process.env.KAHAWAI_E2E_PUBLIC_PORT ?? 18430)
const controlPort = Number(process.env.KAHAWAI_E2E_CONTROL_PORT ?? 18433)

export default defineConfig({
  testDir: './test/browser/product',
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 1 : 0,
  timeout: 90_000,
  expect: { timeout: 15_000 },
  reporter: process.env.CI ? [['line'], ['html', { open: 'never' }]] : 'line',
  outputDir: 'test-results/product',
  use: {
    baseURL: `http://127.0.0.1:${publicPort}`,
    reducedMotion: 'reduce',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    video: 'retain-on-failure',
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'], browserName: 'chromium', channel: 'chrome' },
    },
    {
      name: 'webkit',
      use: { ...devices['Desktop Safari'], browserName: 'webkit' },
    },
  ],
  webServer: {
    command: 'cargo run --quiet -p kahawai --example product_browser_fixture',
    cwd: '..',
    url: `http://127.0.0.1:${controlPort}/ready`,
    reuseExistingServer: false,
    timeout: 180_000,
  },
})
