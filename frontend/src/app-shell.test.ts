// @vitest-environment happy-dom
/**
 * Shell audit: the minimal frame after the template cleanup — brand-only
 * sidebar, a single dashboard route, unknown paths falling back to the
 * dashboard. The session-workbench views were deleted; any route beyond
 * `/` must come back together with the `webui-ui` capability work.
 */

import { describe, expect, it } from 'vitest'
import { matchRoute } from './router.js'
import { ROUTES } from './app-shell.js'

describe('shell route surface', () => {
  it('ships exactly the dashboard route', () => {
    expect(ROUTES).toEqual([{ id: 'dashboard', pattern: '/' }])
  })

  it('resolves `/` and falls back for anything else', () => {
    expect(matchRoute(ROUTES, '/')?.id).toBe('dashboard')
    expect(matchRoute(ROUTES, '/sessions')).toBeNull()
  })
})
