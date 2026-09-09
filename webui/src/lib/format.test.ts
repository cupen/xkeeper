import { describe, expect, it } from 'vitest'
import {
  formatBytes,
  formatPercent,
  formatRate,
  formatUptime,
  formatWindow,
  stateGlyph,
} from './format.js'

describe('format', () => {
  it('never fakes 0 for missing data', () => {
    expect(formatBytes(null)).toBe('—')
    expect(formatPercent(undefined)).toBe('—')
    expect(formatRate(null)).toBe('—')
    expect(formatUptime(null)).toBe('—')
  })

  it('humanizes byte sizes', () => {
    expect(formatBytes(0)).toBe('0 B')
    expect(formatBytes(512)).toBe('512 B')
    expect(formatBytes(2048)).toBe('2.0 KiB')
    expect(formatBytes(5 * 1024 * 1024)).toBe('5.0 MiB')
    expect(formatBytes(3.3 * 1024 ** 3)).toBe('3.3 GiB')
    expect(formatBytes(2.5 * 1024 ** 4)).toBe('2.5 TiB')
  })

  it('formats percentages with one decimal', () => {
    expect(formatPercent(0)).toBe('0.0%')
    expect(formatPercent(37.44)).toBe('37.4%')
    expect(formatPercent(100)).toBe('100.0%')
  })

  it('formats rates', () => {
    expect(formatRate(0)).toBe('0/s')
    expect(formatRate(5.25)).toBe('5.3/s')
    expect(formatRate(48123)).toBe('48,123/s')
  })

  it('formats uptimes in coarse units', () => {
    expect(formatUptime(0.4)).toBe('0s')
    expect(formatUptime(3.14)).toBe('3.1s')
    expect(formatUptime(65)).toBe('1m 5s')
    expect(formatUptime(3600 * 2 + 60)).toBe('2h 1m')
    expect(formatUptime(86400 + 3661)).toBe('1d 1h 1m')
  })

  it('formats rate windows', () => {
    expect(formatWindow(1)).toBe('1s')
    expect(formatWindow(10)).toBe('10s')
    expect(formatWindow(60)).toBe('1m')
    expect(formatWindow(300)).toBe('5m')
  })

  it('gives every lifecycle state a distinct glyph', () => {
    const states = ['starting', 'running', 'stopping', 'backoff', 'exited', 'stopped', 'fatal']
    const glyphs = new Set(states.map(stateGlyph))
    expect(glyphs.size).toBe(states.length)
    expect(stateGlyph('unknown-state')).toBe('•')
  })
})
