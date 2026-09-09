// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import './console-tree.js'
import './overview-bar.js'
import '../views/program-detail.js'
import type { XkeeperConsoleTree } from './console-tree.js'
import type { XkeeperOverviewBar } from './overview-bar.js'
import type { XkeeperProgramDetail } from '../views/program-detail.js'
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

function program(app: string, name: string, state: string, metrics = false): ProgramInfo {
  return {
    app,
    name,
    state,
    pid: state === 'running' ? 42 : null,
    unhealthy: false,
    uptime_secs: 120,
    total_exits: 3,
    restart_backoff: 1,
    last_exit: null,
    fatal_reason: state === 'fatal' ? 'retries exhausted' : null,
    wait_reason: null,
    cpu_percent: metrics && state === 'running' ? 12.5 : null,
    mem_bytes: metrics && state === 'running' ? 4096 : null,
    log_rate: {
      out: { w1: 1, w10: 2, w60: 3, w300: 4 },
      err: { w1: 0, w10: 0, w60: 0, w300: 0 },
    },
    command: 'sh',
    args: ['-c', 'while :; do echo tick; sleep 1; done'],
    work_dir: '/srv/demo',
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
      system: { cpu_percent: 21.5, mem_used_bytes: 4 * 1024 ** 3, mem_total_bytes: 16 * 1024 ** 3 },
      monitor_interval: 1,
      uptime_secs: 90,
      config_source: '/tmp/xkeeper.toml',
    },
    programs: [
      program('demo', 'web', 'running', true),
      program('demo', 'worker', 'fatal', false),
      program('demo', 'never-started', 'stopped', false),
    ],
  }
}

describe('console components', () => {
  let store: ConsoleStore
  let ws: FakeWebSocket

  beforeEach(async () => {
    vi.stubGlobal('WebSocket', FakeWebSocket as unknown as typeof WebSocket)
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => doc() }))
    FakeWebSocket.instances = []
    localStorage.clear()
    store = new ConsoleStore()
    store.start()
    ws = FakeWebSocket.instances[0]!
    ws.serverOpen()
    ws.serverFrame(snapshotFrame(doc()))
    await new Promise((r) => setTimeout(r, 0))
  })

  afterEach(() => {
    store.stop()
    vi.useRealTimers()
    vi.unstubAllGlobals()
    vi.restoreAllMocks()
  })

  it('tree lists every program including never-started ones, with active highlighting', async () => {
    const el = document.createElement('xkeeper-console-tree') as XkeeperConsoleTree
    el.apps = [{ name: 'demo', programs: doc().programs }]
    el.selectedApp = 'demo'
    el.selectedProgram = 'web'
    document.body.appendChild(el)
    await el.updateComplete

    const links = [...el.shadowRoot!.querySelectorAll('a.prog')]
    expect(links.map((a) => a.textContent)).toHaveLength(3)
    expect(links.map((a) => a.getAttribute('data-state'))).toEqual(['running', 'fatal', 'stopped'])
    const active = el.shadowRoot!.querySelector('a.prog[data-active="true"]')
    expect(active?.textContent).toContain('web')
    // never-started program renders a name, not an error
    expect(el.shadowRoot!.textContent).toContain('never-started')
    el.remove()
  })

  it('overview bar shows system metrics and the refresh selector persists', async () => {
    const el = document.createElement('xkeeper-overview-bar') as XkeeperOverviewBar
    el.store = store
    document.body.appendChild(el)
    await el.updateComplete
    // store already has the snapshot
    await new Promise((r) => setTimeout(r, 0))
    await el.updateComplete

    const text = el.shadowRoot!.textContent!
    expect(text).toContain('0.1.0')
    expect(text).toContain('/tmp/xkeeper.toml')
    expect(text).toContain('21.5%')
    expect(text).toContain('4.0 GiB')
    const select = el.shadowRoot!.querySelector('#refresh-select') as HTMLSelectElement
    expect(select).not.toBeNull()
    // user picks 10s → persisted
    select.value = '10000'
    select.dispatchEvent(new Event('change'))
    await el.updateComplete
    expect(localStorage.getItem('xkeeper.refresh_ms')).toBe('10000')
    expect(store.refreshInterval).toBe(10000)
    el.remove()
  })

  it('program detail shows run info and requires confirmation for stop', async () => {
    const el = document.createElement('xkeeper-program-detail') as XkeeperProgramDetail
    el.store = store
    el.app = 'demo'
    el.program = 'web'
    document.body.appendChild(el)
    await el.updateComplete
    await new Promise((r) => setTimeout(r, 0))
    await el.updateComplete

    const text = el.shadowRoot!.textContent!
    expect(text).toContain('sh -c while :; do echo tick; sleep 1; done')
    expect(text).toContain('42')
    expect(text).toContain('12.5%')
    expect(text).toContain('4.0 KiB')

    // fetch mock tracks the control call
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => ({ result: 'program web is stopped' }),
    })
    vi.stubGlobal('fetch', fetchMock)

    const stopButton = [...el.shadowRoot!.querySelectorAll('button')].find(
      (b) => b.textContent?.trim() === '停止',
    )
    expect(stopButton).toBeDefined()
    stopButton!.click()
    await el.updateComplete
    // confirmation step: no request yet
    expect(fetchMock).not.toHaveBeenCalled()
    const confirmButton = [...el.shadowRoot!.querySelectorAll('button')].find(
      (b) => b.textContent?.trim() === '确认',
    )
    confirmButton!.click()
    await new Promise((r) => setTimeout(r, 0))
    await el.updateComplete
    expect(fetchMock).toHaveBeenCalledWith('/api/programs/web/stop', { method: 'POST' })
    expect(el.shadowRoot!.textContent).toContain('program web is stopped')
    el.remove()
  })

  it('program detail surfaces failure feedback on 409', async () => {
    const el = document.createElement('xkeeper-program-detail') as XkeeperProgramDetail
    el.store = store
    el.app = 'demo'
    el.program = 'worker' // fatal state → start button offered
    document.body.appendChild(el)
    await el.updateComplete
    await new Promise((r) => setTimeout(r, 0))
    await el.updateComplete

    const fetchMock = vi.fn().mockResolvedValue({
      ok: false,
      status: 409,
      json: async () => ({ error: 'cannot start: retries exhausted' }),
    })
    vi.stubGlobal('fetch', fetchMock)

    const startButton = [...el.shadowRoot!.querySelectorAll('button')].find(
      (b) => b.textContent?.trim() === '启动',
    )
    expect(startButton).toBeDefined()
    startButton!.click()
    await new Promise((r) => setTimeout(r, 0))
    await el.updateComplete
    expect(fetchMock).toHaveBeenCalledWith('/api/programs/worker/start', { method: 'POST' })
    expect(el.shadowRoot!.textContent).toContain('cannot start: retries exhausted')
    el.remove()
  })
})
