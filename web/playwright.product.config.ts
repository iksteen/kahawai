import { defineConfig, devices } from '@playwright/test'

import {
  CONTROL,
  CONTROL_ADDRESS,
  PUBLIC,
  PUBLIC_ADDRESS,
  SATELLITE_ADDRESS,
  SETUP_ADDRESS,
} from './test/browser/product/addresses.ts'

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
    baseURL: PUBLIC,
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
    command: 'cargo run --quiet --locked -p kahawai --example product_browser_fixture',
    cwd: '..',
    env: {
      KAHAWAI_E2E_PUBLIC: PUBLIC_ADDRESS,
      KAHAWAI_E2E_SETUP: SETUP_ADDRESS,
      KAHAWAI_E2E_SATELLITE: SATELLITE_ADDRESS,
      KAHAWAI_E2E_CONTROL: CONTROL_ADDRESS,
    },
    url: `${CONTROL}/ready`,
    reuseExistingServer: false,
    timeout: 180_000,
  },
})
