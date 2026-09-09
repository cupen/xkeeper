/**
 * Display formatting helpers. Pure functions — every one is trivially
 * testable and none touch the DOM.
 */

/** 1234567 → "1.2 MiB"; null/undefined → "—" (never fake 0). */
export function formatBytes(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined || Number.isNaN(bytes)) return '—'
  if (bytes < 1024) return `${bytes} B`
  const units = ['KiB', 'MiB', 'GiB', 'TiB']
  let value = bytes / 1024
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit++
  }
  return `${value >= 100 ? value.toFixed(0) : value.toFixed(1)} ${units[unit]}`
}

/** 12.34 → "12.3%"; null → "—" (no data is not 0%). */
export function formatPercent(pct: number | null | undefined): string {
  if (pct === null || pct === undefined || Number.isNaN(pct)) return '—'
  return `${pct.toFixed(1)}%`
}

/** Lines-per-second; null → "—". Values ≥ 1000 lose the decimals. */
export function formatRate(linesPerSec: number | null | undefined): string {
  if (linesPerSec === null || linesPerSec === undefined || Number.isNaN(linesPerSec)) return '—'
  if (linesPerSec >= 1000) return `${Math.round(linesPerSec).toLocaleString()}/s`
  if (linesPerSec === 0) return '0/s'
  return `${linesPerSec.toFixed(1)}/s`
}

/** 65.4 → "1m 5s"; 0 → "0s". Small seconds keep one decimal under 10s. */
export function formatUptime(secs: number | null | undefined): string {
  if (secs === null || secs === undefined || Number.isNaN(secs)) return '—'
  if (secs < 1) return '0s'
  if (secs < 10) return `${secs.toFixed(1)}s`
  const total = Math.floor(secs)
  const d = Math.floor(total / 86400)
  const h = Math.floor((total % 86400) / 3600)
  const m = Math.floor((total % 3600) / 60)
  const s = total % 60
  const parts: string[] = []
  if (d > 0) parts.push(`${d}d`)
  if (h > 0) parts.push(`${h}h`)
  if (m > 0) parts.push(`${m}m`)
  if (parts.length === 0 || (d === 0 && h === 0 && s > 0)) parts.push(`${s}s`)
  return parts.join(' ')
}

/** Window seconds → short label ("10s", "1m", "5m"). */
export function formatWindow(secs: number): string {
  if (secs < 60) return `${secs}s`
  if (secs < 3600) return `${secs / 60}m`
  return `${secs / 3600}h`
}

/** A label for the backend lifecycle states (glyph + word, a11y). */
export function stateGlyph(state: string): string {
  switch (state) {
    case 'running':
      return '▶'
    case 'starting':
      return '◌'
    case 'stopping':
      return '◍'
    case 'backoff':
      return '↻'
    case 'exited':
      return '■'
    case 'stopped':
      return '⏹'
    case 'fatal':
      return '✖'
    default:
      return '•'
  }
}
