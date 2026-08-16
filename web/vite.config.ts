import tailwindcss from '@tailwindcss/vite'
import vue from '@vitejs/plugin-vue'
import { defineConfig } from 'vite'

export default defineConfig({
  base: '/app/',
  plugins: [vue(), tailwindcss()],
  server: {
    // Both prefixes, or the admin screens 404 under `npm run dev` while every
    // other screen works — which reads as an admin bug. Carried over from
    // `web/`, where that was learned.
    proxy: {
      '/api': 'http://localhost:8420',
      '/admin': 'http://localhost:8420',
    },
  },
})
