// Entry point: registers the app shell and the placeholder dashboard.
// The console surface (program overview, controls, logs) is specified by
// the `webui-ui` capability and lands in follow-up changes.

import './app-shell.js'
import './views/dashboard.js'

// Web Awesome theme + base styles (self-hosted, no CDN).
import '@awesome.me/webawesome/dist/styles/webawesome.css'
import '@awesome.me/webawesome/dist/styles/themes/default.css'
// xkeeper's theme mapping on top of Web Awesome (indigo brand, dark surfaces).
import './styles/wa-overrides.css'

// Theme: `wa-dark` on <html> is the single switch (dark is the default; the
// mode lives in src/theme.ts and index.html applies it before first paint).
// System mode live-follows an OS preference change.
import { applyThemeMode } from './theme.js'
applyThemeMode()
if (typeof window.matchMedia === 'function') {
  window
    .matchMedia('(prefers-color-scheme: light)')
    .addEventListener('change', applyThemeMode)
}
