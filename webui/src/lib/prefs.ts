/**
 * User preferences (localStorage-backed). The console has exactly three:
 * - refresh interval for list/metric display updates (log following is
 *   never throttled — webui-ui: 刷新频率控制),
 * - the log-rate window the rate columns show,
 * - nothing else — resist feature creep here.
 */

export const REFRESH_CHOICES_MS = [1000, 3000, 5000, 10000] as const
export type RefreshMs = (typeof REFRESH_CHOICES_MS)[number]

export const RATE_WINDOW_CHOICES_S = [1, 10, 60, 300] as const
export type RateWindowS = (typeof RATE_WINDOW_CHOICES_S)[number]

const REFRESH_KEY = 'xkeeper.refresh_ms'
const WINDOW_KEY = 'xkeeper.rate_window_s'

function loadNumber(key: string, allowed: readonly number[], fallback: number): number {
  try {
    const raw = localStorage.getItem(key)
    if (raw === null) return fallback
    const v = Number(raw)
    return allowed.includes(v) ? v : fallback
  } catch {
    return fallback
  }
}

export function loadRefreshMs(): RefreshMs {
  return loadNumber(REFRESH_KEY, REFRESH_CHOICES_MS, 3000) as RefreshMs
}

export function saveRefreshMs(ms: RefreshMs): void {
  try {
    localStorage.setItem(REFRESH_KEY, String(ms))
  } catch {
    /* private mode etc: preferences become session-only */
  }
}

export function loadRateWindowS(): RateWindowS {
  return loadNumber(WINDOW_KEY, RATE_WINDOW_CHOICES_S, 10) as RateWindowS
}

export function saveRateWindowS(s: RateWindowS): void {
  try {
    localStorage.setItem(WINDOW_KEY, String(s))
  } catch {
    /* session-only */
  }
}
