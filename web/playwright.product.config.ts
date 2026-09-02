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
  // The journey deliberately mutates one retained catalog (setup, grants,
  // then a crash-style restart). Playwright keeps webServer alive across a
  // retry, so rerunning the serial group would inherit that state and fail for
  // the wrong reason. A fresh CI job is the only honest retry boundary.
  retries: 0,
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
    // Product failures are usually a refusal inside the supervised Rust/media
    // worker. Playwright pipes stderr by default but discards stdout, which is
    // where tracing-subscriber writes the refusal chain and session id.
    stdout: 'pipe',
    env: {
      KAHAWAI_E2E_PUBLIC: PUBLIC_ADDRESS,
      KAHAWAI_E2E_SETUP: SETUP_ADDRESS,
      KAHAWAI_E2E_SATELLITE: SATELLITE_ADDRESS,
      KAHAWAI_E2E_CONTROL: CONTROL_ADDRESS,
    },
    url: `${CONTROL}/ready`,
    reuseExistingServer: false,
    // The macOS release gate starts without a workspace build. A cold Rust +
    // GStreamer compile may take minutes; this ceiling covers startup only.
    timeout: 600_000,
    gracefulShutdown: { signal: 'SIGTERM', timeout: 30_000 },
  },
})
