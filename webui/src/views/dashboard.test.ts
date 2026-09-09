// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from 'vitest'
import './dashboard.js'
import type { XkeeperDashboard } from './dashboard.js'
import { ConsoleStore } from '../lib/store.js'
import { MSG, type StatusDoc } from '../lib/types.js'
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
  serverClose(): void {
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

function snapshotFrame(doc: StatusDoc): Uint8Array {
  const payload = encode(doc)
  const out = new Uint8Array(payload.length + 1)
  out[0] = MSG.SNAPSHOT
  out.set(payload, 1)
  return out
}

function doc(): StatusDoc {
  return {
    daemon: {
      version: '0.1.0',
      port: 9877,
      apps: 1,
      system: { cpu_percent: 3, mem_used_bytes: 1, mem_total_bytes: 2 },
      monitor_interval: 1,
      uptime_secs: 5,
      config_source: '/t',
    },
    programs: [
      program('healthy', 'running', null),
      program('sick', 'fatal', 'start failed 5 times'),
    ],
  }
}

function program(name: string, state: string, fatalReason: string | null): StatusDoc['programs'][number] {
  return {
    app: 'demo',
    name,
    state,
    pid: state === 'running' ? 123 : null,
    unhealthy: false,
    uptime_secs: 1,
    total_exits: 0,
    restart_backoff: 1,
    last_exit: null,
    fatal_reason: fatalReason,
    wait_reason: null,
    cpu_percent: null,
    mem_bytes: null,
    log_rate: { out: { w1: 0, w10: 0, w60: 0, w300: 0 }, err: { w1: 0, w10: 0, w60: 0, w300: 0 } },
    command: 'x',
    args: [],
    work_dir: '',
  }
}

describe('dashboard (console overview)', () => {
  afterEach(() => {
    vi.useRealTimers()
    vi.unstubAllGlobals()
    vi.restoreAllMocks()
  })

  function freshStore(): { store: ConsoleStore; ws: FakeWebSocket } {
    vi.stubGlobal('WebSocket', FakeWebSocket as unknown as typeof WebSocket)
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue({ ok: true, json: async () => doc() }),
    )
    FakeWebSocket.instances = []
    const store = new ConsoleStore()
    store.start()
    const ws = FakeWebSocket.instances[0]!
    ws.serverOpen()
    return { store, ws }
  }

  async function mount(store: ConsoleStore): Promise<XkeeperDashboard> {
    const el = document.createElement('xkeeper-dashboard') as XkeeperDashboard
    el.store = store
    document.body.appendChild(el)
    await el.updateComplete
    return el
  }

  it('renders app sections with program states from the live snapshot', async () => {
    const { store, ws } = freshStore()
    const el = await mount(store)
    ws.serverFrame(snapshotFrame(doc()))
    await new Promise((r) => setTimeout(r, 0))
    await el.updateComplete

    expect(el.shadowRoot?.textContent).toContain('demo')
    expect(el.shadowRoot?.textContent).toContain('healthy')
    expect(el.shadowRoot?.textContent).toContain('sick')
    expect(el.shadowRoot?.textContent).toContain('start failed 5 times')
    const fatalRow = el.shadowRoot?.querySelector('li[data-state="fatal"]')
    expect(fatalRow).not.toBeNull()
    expect(el.shadowRoot?.querySelector('.banner[data-mode="offline"]')).toBeNull()
    store.stop()
    el.remove()
  })

  it('shows the offline banner with retained data when the socket dies', async () => {
    const { store, ws } = freshStore()
    const el = await mount(store)
    ws.serverFrame(snapshotFrame(doc()))
    await new Promise((r) => setTimeout(r, 0))
    ws.serverClose()
    await new Promise((r) => setTimeout(r, 0))
    await el.updateComplete

    // Poll fallback produced fresh data (fetch mocked ok) → poll banner,
    // and the app list stays visible.
    expect(el.shadowRoot?.querySelector('.banner[data-mode="poll"]')).not.toBeNull()
    expect(el.shadowRoot?.textContent).toContain('healthy')
    store.stop()
    el.remove()
  })
})
