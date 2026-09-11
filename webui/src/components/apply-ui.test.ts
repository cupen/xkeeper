// @vitest-environment happy-dom
//
// webui-ui: pending 变更提示与 apply 操作 — component-level interaction
// tests mirroring the acceptance checklist (F) against the real Lit
// components with a fake transport. The full-browser pass lives in
// `cargo run -p xtask -- e2e` (playwright).

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import './overview-bar.js'
import '../views/app-overview.js'
import type { XkeeperOverviewBar } from './overview-bar.js'
import type { XkeeperAppOverview } from '../views/app-overview.js'
import { ConsoleStore } from '../lib/store.js'
import { MSG, type ProgramInfo, type StatusDoc } from '../lib/types.js'
import { encode } from '@msgpack/msgpack'

class FakeWebSocket {
  static OPEN = 1
  static CONNECTING = 0
  static CLOSED = 3
  static instances: FakeWebSocket[] = []
  readyState = FakeWebSocket.CONNECTING
  sent: Uint8Array[] = []
  onopen: (() => void) | null = null
  onclose: (() => void) | null = null
  onerror: (() => void) | null = null
  onmessage: ((ev: { data: ArrayBuffer }) => void) | null = null
  constructor(public url: string) {
    FakeWebSocket.instances.push(this)
  }
  send(data: unknown): void {
    this.sent.push(data as Uint8Array)
  }
  close(): void {
    this.readyState = FakeWebSocket.CLOSED
    this.onclose?.()
  }
  serverOpen(): void {
    this.readyState = FakeWebSocket.OPEN
    this.onopen?.()
  }
  serverFrame(frame: Uint8Array): void {
    this.onmessage?.({ data: new Uint8Array(frame).buffer })
  }
}

function program(app: string, name: string, state = 'running'): ProgramInfo {
  return {
    app,
    name,
    state,
    pid: state === 'running' ? 42 : null,
    unhealthy: false,
    uptime_secs: 120,
    total_exits: 0,
    restart_backoff: 1,
    last_exit: null,
    fatal_reason: null,
    wait_reason: null,
    cpu_percent: null,
    mem_bytes: null,
    log_rate: {
      out: { w1: 0, w10: 0, w60: 0, w300: 0 },
      err: { w1: 0, w10: 0, w60: 0, w300: 0 },
    },
    command: 'sleep 300',
    args: [],
    work_dir: '',
  }
}

function emptyPending(): StatusDoc['pending'] {
  return { programs: [], apps_added: [], apps_removed: [], daemon_hints: [], errors: [] }
}

function snapshotFrame(doc: StatusDoc): Uint8Array {
  const payload = encode(doc)
  const out = new Uint8Array(payload.length + 1)
  out[0] = MSG.SNAPSHOT
  out.set(payload, 1)
  return out
}

/** STATUS frame as the server sends it: JSON text {programs, pending?}. */
function statusFrame(programs: ProgramInfo[], pending?: StatusDoc['pending']): Uint8Array {
  const obj: Record<string, unknown> = { programs }
  if (pending) obj['pending'] = pending
  const text = new TextEncoder().encode(JSON.stringify(obj))
  const out = new Uint8Array(text.length + 1)
  out[0] = MSG.STATUS
  out.set(text, 1)
  return out
}

function baseDoc(): StatusDoc {
  return {
    daemon: {
      version: '0.1.0',
      port: 9877,
      apps: 2,
      system: { cpu_percent: 5, mem_used_bytes: 1, mem_total_bytes: 2 },
      monitor_interval: 1,
      uptime_secs: 10,
      config_source: '/t',
    },
    pending: emptyPending(),
    programs: [program('alpha', 'web'), program('alpha', 'worker'), program('beta', 'job')],
  }
}

describe('pending + apply UI (webui-ui: pending 变更提示与 apply 操作)', () => {
  let store: ConsoleStore
  let ws: FakeWebSocket
  let fetchMock: ReturnType<typeof vi.fn>

  beforeEach(async () => {
    vi.stubGlobal('WebSocket', FakeWebSocket as unknown as typeof WebSocket)
    fetchMock = vi.fn().mockResolvedValue({ ok: true, json: async () => baseDoc() })
    vi.stubGlobal('fetch', fetchMock)
    FakeWebSocket.instances = []
    store = new ConsoleStore()
    store.start()
    ws = FakeWebSocket.instances[0]!
    ws.serverOpen()
    ws.serverFrame(snapshotFrame(baseDoc()))
    await new Promise((r) => setTimeout(r, 0))
  })

  afterEach(() => {
    store.stop()
    vi.useRealTimers()
    vi.unstubAllGlobals()
    vi.restoreAllMocks()
  })

  async function mountBar(): Promise<XkeeperOverviewBar> {
    const el = document.createElement('xkeeper-overview-bar') as XkeeperOverviewBar
    el.store = store
    document.body.appendChild(el)
    await el.updateComplete
    return el
  }

  async function mountOverview(app: string): Promise<XkeeperAppOverview> {
    const el = document.createElement('xkeeper-app-overview') as XkeeperAppOverview
    el.store = store
    el.app = app
    document.body.appendChild(el)
    await el.updateComplete
    return el
  }

  function flushFrames(): Promise<void> {
    return new Promise((r) => setTimeout(r, 0))
  }

  it('badge appears when a pending change arrives over WS and disappears after apply', async () => {
    const bar = await mountBar()
    expect(bar.shadowRoot!.querySelector('.pending')).toBeNull()

    // Disk edit detected server-side → pending delta.
    ws.serverFrame(statusFrame([], {
      ...emptyPending(),
      programs: [{ app: 'alpha', program: 'worker', running: true }],
    }))
    await flushFrames()
    await bar.updateComplete
    await bar.updateComplete

    const strip = bar.shadowRoot!.querySelector('.pending')
    expect(strip).not.toBeNull()
    expect(strip!.textContent).toContain('待应用变更')
    expect(strip!.textContent).toContain('1 个程序配置有变化')

    // Apply posts, the server clears pending in a STATUS delta → strip gone.
    fetchMock.mockResolvedValueOnce({
      ok: true,
      json: async () => ({ result: 'changed:\n  alpha.worker -> update-and-restart' }),
    })
    vi.stubGlobal('confirm', vi.fn().mockReturnValue(true))
    bar.shadowRoot!.querySelector<HTMLButtonElement>('.pending button')!.click()
    await flushFrames()
    await bar.updateComplete

    const [url, init] = fetchMock.mock.calls.at(-1) as unknown as [string, RequestInit]
    expect(url).toBe('/api/apply')
    expect(JSON.parse(String(init.body))).toEqual({ restart: false })
    expect(bar.shadowRoot!.textContent).toContain('update-and-restart')

    ws.serverFrame(statusFrame([], emptyPending()))
    await flushFrames()
    await bar.updateComplete
    await bar.updateComplete
    // The strip host may stay up to show the result text, but the badge and
    // the count are gone — pending is cleared.
    expect(bar.shadowRoot!.querySelector('.pending .badge')).toBeNull()
    expect(bar.shadowRoot!.textContent).not.toContain('待应用变更')
    bar.remove()
  })

  it('apply is a no-op click without confirmation (user declines)', async () => {
    const bar = await mountBar()
    ws.serverFrame(statusFrame([], {
      ...emptyPending(),
      programs: [{ app: 'alpha', program: 'web', running: true }],
    }))
    await flushFrames()
    await bar.updateComplete
    await bar.updateComplete

    vi.stubGlobal('confirm', vi.fn().mockReturnValue(false))
    bar.shadowRoot!.querySelector<HTMLButtonElement>('.pending button')!.click()
    await flushFrames()
    expect(fetchMock).not.toHaveBeenCalledWith('/api/apply', expect.anything())
    bar.remove()
  })

  it('app overview marks changed rows and scopes its apply to that app only', async () => {
    const view = await mountOverview('alpha')
    expect(view.shadowRoot!.querySelector('.changed-mark')).toBeNull()

    // Both alpha and beta have pending changes.
    ws.serverFrame(statusFrame([], {
      ...emptyPending(),
      programs: [
        { app: 'alpha', program: 'worker', running: true },
        { app: 'beta', program: 'job', running: true },
      ],
    }))
    await flushFrames()
    await view.updateComplete
    await view.updateComplete

    // Only alpha rows carry the mark.
    const marks = view.shadowRoot!.querySelectorAll('.changed-mark')
    expect(marks).toHaveLength(1)
    expect(marks[0]!.closest('tr')!.textContent).toContain('worker')

    // App-scope apply sends only this app.
    fetchMock.mockResolvedValueOnce({
      ok: true,
      json: async () => ({ result: 'changed:\n  alpha.worker -> update-and-restart' }),
    })
    vi.stubGlobal('confirm', vi.fn().mockReturnValue(true))
    view.shadowRoot!.querySelector<HTMLButtonElement>('.apply-strip button')!.click()
    await flushFrames()
    await view.updateComplete

    const [url, init] = fetchMock.mock.calls.at(-1) as unknown as [string, RequestInit]
    expect(url).toBe('/api/apply')
    expect(JSON.parse(String(init.body))).toEqual({ restart: false, app: 'alpha' })
    expect(view.shadowRoot!.textContent).toContain('update-and-restart')

    // Server applies alpha only → beta stays pending; the strip is still up
    // for the other app's change even though alpha's rows cleared.
    ws.serverFrame(statusFrame([], {
      ...emptyPending(),
      programs: [{ app: 'beta', program: 'job', running: true }],
    }))
    await flushFrames()
    await view.updateComplete
    expect(view.shadowRoot!.querySelector('.apply-strip')).not.toBeNull()
    expect(view.shadowRoot!.querySelector('.changed-mark')).toBeNull()
    view.remove()
  })

  it('apply failure surfaces the API error instead of a silent pass', async () => {
    const view = await mountOverview('beta')
    ws.serverFrame(statusFrame([], {
      ...emptyPending(),
      programs: [{ app: 'beta', program: 'job', running: true }],
    }))
    await flushFrames()
    await view.updateComplete
    await view.updateComplete

    fetchMock.mockResolvedValueOnce({
      ok: false,
      json: async () => ({ error: 'apply aborted, daemon config invalid' }),
    })
    vi.stubGlobal('confirm', vi.fn().mockReturnValue(true))
    view.shadowRoot!.querySelector<HTMLButtonElement>('.apply-strip button')!.click()
    await flushFrames()
    await view.updateComplete

    const feedback = view.shadowRoot!.querySelector('.apply-strip .feedback')
    expect(feedback).not.toBeNull()
    expect(feedback!.getAttribute('data-error')).toBe('true')
    expect(feedback!.textContent).toContain('daemon config invalid')
    view.remove()
  })
})
