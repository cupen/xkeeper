import { defineConfig } from 'vitest/config'

// Dev workflow: two processes. The Rust backend (axum) serves the JSON API,
// health check, and WebSocket push on 127.0.0.1:9877 (override with
// XKEEPER_WEBUI_DEV_BACKEND); Vite serves the SPA here with hot reload and
// proxies the backend surfaces so the app is deployed and developed against
// identical paths.
const backend = process.env.XKEEPER_WEBUI_DEV_BACKEND ?? 'http://127.0.0.1:9877'

export default defineConfig({
  server: {
    // Dedicated port: other projects on this machine run their own Vite on
    // the default 5173 — a fixed, non-default port avoids cross-project
    // collisions and keeps the URL stable across restarts.
    port: 5273,
    strictPort: true,
    proxy: {
      '/api': { target: backend, changeOrigin: false },
      '/health': { target: backend },
      '/ws': { target: backend, ws: true },
    },
  },
  build: {
    outDir: 'dist',
    sourcemap: false,
  },
  test: {
    environment: 'happy-dom',
    include: ['src/**/*.test.ts'],
  },
})
