import { defineConfig, mergeConfig } from 'vitest/config'

import viteConfig from './vite.config.ts'

/// Separate from `vite.config.ts` because vite's own `defineConfig` has no
/// `test` field — putting one there typechecks as an unknown property and is
/// silently ignored by everything except vitest.
export default mergeConfig(
  viteConfig,
  defineConfig({
    test: {
      // Components need a DOM, and this is the cheap one.
      environment: 'happy-dom',
      include: ['test/**/*.test.ts'],
    },
  }),
)
