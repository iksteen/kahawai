import { defineConfig } from 'orval'

export default defineConfig({
  kahawai: {
    input: {
      // The document `web/` already generates from the hub and gates with a
      // fingerprint. One document: a copy here would be a second thing to keep
      // in step, and the whole point of generating it is that nobody has to.
      // At cutover this directory takes `web/`'s name and the path becomes
      // `./openapi.json` like the old one.
      target: process.env.KAHAWAI_OPENAPI ?? '../web/openapi.json',
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
