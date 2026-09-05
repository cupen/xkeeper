// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from 'vitest'
import './dashboard.js'
import type { XkeeperDashboard } from './dashboard.js'

describe('placeholder dashboard', () => {
  const originalFetch = globalThis.fetch

  afterEach(() => {
    globalThis.fetch = originalFetch
    vi.restoreAllMocks()
  })

  async function mount(): Promise<XkeeperDashboard> {
    const el = document.createElement('xkeeper-dashboard') as XkeeperDashboard
    document.body.appendChild(el)
    await el.updateComplete
    return el
  }

  it('reports a reachable backend from GET /api/health', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue({ ok: true })
    const el = await mount()
    const health = el.shadowRoot?.querySelector('.health')
    expect(globalThis.fetch).toHaveBeenCalledWith('/api/health')
    expect(health?.getAttribute('data-up')).toBe('true')
    expect(health?.textContent).toContain('/api/health ok')
    el.remove()
  })

  it('reports an unreachable backend without faking data', async () => {
    globalThis.fetch = vi.fn().mockRejectedValue(new Error('refused'))
    const el = await mount()
    const health = el.shadowRoot?.querySelector('.health')
    expect(health?.getAttribute('data-up')).toBe('false')
    expect(health?.textContent).toContain('后端不可达')
    el.remove()
  })
})
