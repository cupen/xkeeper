import { describe, expect, it } from 'vitest'
import { matchRoute } from './router.js'

// Production routes (app-shell.ts ROUTES): the placeholder console ships a
// single route; future capability work appends here.
const ROUTES = [{ id: 'dashboard', pattern: '/' }]

describe('matchRoute', () => {
  it('matches the dashboard root', () => {
    expect(matchRoute(ROUTES, '/')).toEqual({ id: 'dashboard', params: {} })
  })

  it('returns null for unknown paths (shell falls back to the dashboard)', () => {
    expect(matchRoute(ROUTES, '/sessions')).toBeNull()
    expect(matchRoute(ROUTES, '/admin/users')).toBeNull()
  })

  it('captures :param segments RAW (still percent-encoded)', () => {
    const routes = [{ id: 'detail', pattern: '/programs/:name' }]
    expect(matchRoute(routes, '/programs/my%20svc')).toEqual({
      id: 'detail',
      params: { name: 'my%20svc' },
    })
  })
})
