// @vitest-environment happy-dom
/**
 * Shell audit: the console frame — brand sidebar + console tree + routed
 * outlet. Routes: `/` (dashboard), `/app/:app` (app overview), and
 * `/app/:app/program/:program` (detail). Unknown paths fall back to the
 * dashboard.
 */

import { describe, expect, it } from 'vitest'
import { matchRoute } from './router.js'
import { ROUTES } from './app-shell.js'

describe('shell route surface', () => {
  it('ships the console routes (dashboard, app, program)', () => {
    expect(ROUTES).toEqual([
      { id: 'dashboard', pattern: '/' },
      { id: 'app', pattern: '/app/:app' },
      { id: 'program', pattern: '/app/:app/program/:program' },
    ])
  })

  it('resolves `/` and falls back for anything else', () => {
    expect(matchRoute(ROUTES, '/')?.id).toBe('dashboard')
    expect(matchRoute(ROUTES, '/sessions')).toBeNull()
  })

  it('resolves app and program deep links with encoded params', () => {
    const app = matchRoute(ROUTES, '/app/demo-app')
    expect(app?.id).toBe('app')
    expect(app?.params['app']).toBe('demo-app')
    const prog = matchRoute(ROUTES, '/app/my%20app/program/worker%201')
    expect(prog?.id).toBe('program')
    expect(prog?.params['app']).toBe('my%20app')
    expect(prog?.params['program']).toBe('worker%201')
  })
})
