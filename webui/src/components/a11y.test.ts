// @vitest-environment jsdom
import { describe, expect, it } from 'vitest'
import './status-badge.js'
import type { XkeeperStatusBadge } from './status-badge.js'

describe('accessibility baseline', () => {
  it('status badge text remains available in greyscale (label + glyph nodes)', async () => {
    const el = document.createElement('xkeeper-status-badge') as XkeeperStatusBadge
    el.slug = 'failed'
    el.label = 'Failed'
    el.glyph = '✕'
    document.body.appendChild(el)
    await el.updateComplete
    const label = el.shadowRoot?.querySelector('.label')
    const glyph = el.shadowRoot?.querySelector('.glyph')
    // Shape and word nodes exist: greyscale cannot erase the state.
    expect(label?.textContent).toBe('Failed')
    expect(glyph?.textContent).toBe('✕')
    el.remove()
  })

  it('reduced motion + theme class hooks ship in tokens.css, focus ring in app.css', async () => {
    const fs = await import('node:fs')
    const path = await import('node:path')
    const { fileURLToPath } = await import('node:url')
    const here = path.dirname(fileURLToPath(import.meta.url))
    const tokens = fs.readFileSync(path.join(here, '../../src/styles/tokens.css'), 'utf8')
    expect(tokens).toContain('prefers-reduced-motion')
    // Light re-map is keyed on the single wa-dark switch (see src/theme.ts),
    // so an explicit appearance override can beat the OS preference.
    expect(tokens).toContain(':root:not(.wa-dark)')
    const app = fs.readFileSync(path.join(here, '../../src/styles/app.css'), 'utf8')
    expect(app).toContain(':focus-visible')
  })
})
