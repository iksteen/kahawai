import { defineConfig } from 'orval'

export default defineConfig({
  kahawai: {
    input: {
      // Generated from the hub and gated with a fingerprint — see
      // `scripts/openapi-fingerprint.mjs`. One document: a second copy would be
      // a second thing to keep in step, and the whole point of generating it is
      // that nobody has to.
      target: process.env.KAHAWAI_OPENAPI ?? './openapi.json',
    },
    output: {
      mode: 'split',
      target: './src/api/generated/kahawai.ts',
      schemas: './src/api/generated/model',
      client: 'fetch',
      tsconfig: './tsconfig.app.json',
      clean: true,
      override: {
        fetch: {
          includeHttpResponseReturnType: false,
        },
        mutator: {
          path: './src/api/transport.ts',
          name: 'apiClient',
        },
      },
    },
  },
})
