import { defineConfig } from 'orval'

export default defineConfig({
  kahawai: {
    input: {
      target: process.env.KAHAWAI_OPENAPI ?? './openapi.json',
    },
    output: {
      mode: 'split',
      target: './src/generated/kahawai.ts',
      schemas: './src/generated/model',
      client: 'fetch',
      tsconfig: './tsconfig.app.json',
      clean: true,
      override: {
        fetch: {
          includeHttpResponseReturnType: false,
        },
        mutator: {
          path: './src/api-client.ts',
          name: 'apiClient',
        },
      },
    },
  },
})
