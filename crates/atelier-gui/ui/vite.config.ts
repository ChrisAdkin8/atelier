import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'

// Pinned port + strictPort so Tauri's `devUrl: http://localhost:1420` always
// matches what Vite serves. Without strictPort Vite would silently roll to
// 1421 if 1420 is busy, and Tauri's webview would 404.
export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  server: {
    host: '127.0.0.1',
    port: 1420,
    strictPort: true,
  },
  build: {
    target: ['es2021', 'chrome100', 'safari13'],
    sourcemap: !!process.env.TAURI_DEBUG,
  },
})
