import { beforeEach, describe, expect, it } from 'vitest'
import {
  loadRateWindowS,
  loadRefreshMs,
  saveRateWindowS,
  saveRefreshMs,
  REFRESH_CHOICES_MS,
  RATE_WINDOW_CHOICES_S,
} from './prefs.js'

describe('prefs', () => {
  beforeEach(() => {
    localStorage.clear()
  })

  it('defaults to 3s refresh and 10s rate window', () => {
    expect(loadRefreshMs()).toBe(3000)
    expect(loadRateWindowS()).toBe(10)
  })

  it('persists and reloads a chosen value', () => {
    saveRefreshMs(10000)
    saveRateWindowS(60)
    expect(loadRefreshMs()).toBe(10000)
    expect(loadRateWindowS()).toBe(60)
    // The value is really in localStorage (survives a page reload).
    expect(localStorage.getItem('xkeeper.refresh_ms')).toBe('10000')
  })

  it('falls back to defaults on out-of-vocabulary values', () => {
    localStorage.setItem('xkeeper.refresh_ms', '7000')
    localStorage.setItem('xkeeper.rate_window_s', '42')
    expect(loadRefreshMs()).toBe(3000)
    expect(loadRateWindowS()).toBe(10)
  })

  it('offers exactly the spec choices (webui-ui: 刷新频率控制)', () => {
    expect([...REFRESH_CHOICES_MS]).toEqual([1000, 3000, 5000, 10000])
    expect([...RATE_WINDOW_CHOICES_S]).toEqual([1, 10, 60, 300])
  })
})
