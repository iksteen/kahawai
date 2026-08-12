import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  base: '/app/',
  plugins: [react()],
  server: {
    // Both prefixes, or the admin screens 404 under `npm run dev` while
    // every other screen works — which reads as an admin bug.
    proxy: {
      '/api': 'http://localhost:8420',
      '/admin': 'http://localhost:8420',
    },
  },
})
