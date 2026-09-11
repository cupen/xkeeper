// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ConsoleStore } from './store.js'
import { MSG, type ProgramInfo, type StatusDoc } from './types.js'
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

function frameFor(type: number, payload: Uint8Array): Uint8Array {
  const out = new Uint8Array(payload.length + 1)
  out[0] = type
  out.set(payload, 1)
  return out
}

function doc(programs: ProgramInfo[]): StatusDoc {
  return {
    daemon: {
      version: 't',
      port: 1,
      apps: 1,
      system: { cpu_percent: 1, mem_used_bytes: 1, mem_total_bytes: 2 },
      monitor_interval: 1,
      uptime_secs: 1,
      config_source: '/t',
    },
    programs,
    pending: { programs: [], apps_added: [], apps_removed: [], daemon_hints: [], errors: [] },
  }
}

function prog(name: string, overrides: Partial<ProgramInfo> = {}): ProgramInfo {
  return {
    app: 'a',
    name,
    state: 'running',
    pid: 1,
    unhealthy: false,
    uptime_secs: 1,
    total_exits: 0,
    restart_backoff: 1,
    last_exit: null,
    fatal_reason: null,
    wait_reason: null,
    cpu_percent: null,
    mem_bytes: null,
    log_rate: { out: { w1: 0, w10: 0, w60: 0, w300: 0 }, err: { w1: 0, w10: 0, w60: 0, w300: 0 } },
    command: 'true',
    args: [],
    work_dir: '',
    ...overrides,
  }
}

describe('store', () => {
  beforeEach(() => {
    vi.stubGlobal('WebSocket', FakeWebSocket as unknown as typeof WebSocket)
    FakeWebSocket.instances = []
    localStorage.clear()
  })
  afterEach(() => {
    vi.useRealTimers()
    vi.unstubAllGlobals()
    vi.restoreAllMocks()
  })

  function openStore(): { store: ConsoleStore; ws: FakeWebSocket } {
    const store = new ConsoleStore()
    store.start()
    const ws = FakeWebSocket.instances[0]!
    ws.serverOpen()
    return { store, ws }
  }

  it('merges snapshot and deltas, sorted by app/name', async () => {
    const { store, ws } = openStore()
    ws.serverFrame(frameFor(MSG.SNAPSHOT, encode(doc([prog('b'), prog('a')]) as never)))
    await new Promise((r) => setTimeout(r, 0))
    expect(store.getSnapshot().transport).toBe('live')
    expect(store.getSnapshot().doc!.programs.map((p) => p.name)).toEqual(['a', 'b'])

    // delta replaces by name, keeps the daemon block
    const changed = prog('b', { state: 'backoff', cpu_percent: 9.9 })
    ws.serverFrame(frameFor(MSG.STATUS, encode([changed] as never)))
    await new Promise((r) => setTimeout(r, 0))
    const after = store.getSnapshot().doc!
    expect(after.programs.find((p) => p.name === 'b')!.state).toBe('backoff')
    expect(after.programs.find((p) => p.name === 'b')!.cpu_percent).toBe(9.9)
    expect(after.programs.find((p) => p.name === 'a')!.state).toBe('running')
    expect(after.daemon.version).toBe('t')
    store.stop()
  })

  it('falls back to polling when the socket drops, and flags offline when the API dies', async () => {
    vi.useFakeTimers()
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => doc([prog('a'), prog('new')]),
    })
    vi.stubGlobal('fetch', fetchMock)
    const { store, ws } = openStore()
    ws.serverFrame(frameFor(MSG.SNAPSHOT, encode(doc([prog('a')]) as never)))
    await vi.advanceTimersByTimeAsync(1)
    ws.serverClose() // WS down → degraded polling
    await vi.advanceTimersByTimeAsync(1)
    expect(store.getSnapshot().transport).toBe('poll')

    // Poll succeeds → doc refreshed from /api/overview.
    await vi.advanceTimersByTimeAsync(3000)
    expect(fetchMock).toHaveBeenCalledWith('/api/overview')
    expect(store.getSnapshot().doc!.programs.map((p) => p.name)).toEqual(['a', 'new'])

    // Poll fails → offline banner state; doc retained (stale but present).
    fetchMock.mockRejectedValueOnce(new Error('refused'))
    await vi.advanceTimersByTimeAsync(3000)
    expect(store.getSnapshot().transport).toBe('offline')
    expect(store.getSnapshot().doc).not.toBeNull()

    // WS back → live again.
    const ws2 = FakeWebSocket.instances.at(-1)!
    ws2.serverOpen()
    ws2.serverFrame(frameFor(MSG.SNAPSHOT, encode(doc([prog('a')]) as never)))
    await vi.advanceTimersByTimeAsync(1)
    expect(store.getSnapshot().transport).toBe('live')
    store.stop()
    vi.useRealTimers()
  })

  it('fans out log frames and gaps to viewers, batching per microtask', async () => {
    const { store, ws } = openStore()
    const seen: string[] = []
    const gaps: number[] = []
    store.onLogFrame((f) => seen.push(`${f.program}/${f.stream}:${f.text}`))
    store.onGap((_p, _s, n) => gaps.push(n))
    store.subscribeLogs('p', 0)

    const logFrame = (text: string): Uint8Array => {
      const name = new TextEncoder().encode('p')
      const body = new TextEncoder().encode(text)
      const out = new Uint8Array(1 + 2 + name.length + 1 + body.length)
      let i = 0
      out[i++] = MSG.LOG
      out[i++] = name.length >> 8
      out[i++] = name.length & 0xff
      out.set(name, i)
      i += name.length
      out[i++] = 0
      out.set(body, i)
      return out
    }
    const gapFrame = (): Uint8Array => {
      const name = new TextEncoder().encode('p')
      const out = new Uint8Array(1 + 2 + name.length + 1 + 8)
      let i = 0
      out[i++] = MSG.LOG_GAP
      out[i++] = name.length >> 8
      out[i++] = name.length & 0xff
      out.set(name, i)
      i += name.length
      out[i++] = 0
      new DataView(out.buffer).setBigUint64(i, 42n)
      return out
    }

    ws.serverFrame(logFrame('one\n'))
    ws.serverFrame(logFrame('two\n'))
    ws.serverFrame(gapFrame())
    await new Promise((r) => setTimeout(r, 0))
    expect(seen).toEqual(['p/0:one\n', 'p/0:two\n'])
    expect(gaps).toEqual([42])
    // The subscription action reached the server as JSON.
    expect(JSON.parse(new TextDecoder().decode(ws.sent[0]!))).toEqual({
      action: 'subscribe',
      program: 'p',
      stream: 'out',
    })
    store.unsubscribeLogs('p', 0)
    expect(ws.sent.at(-1)!.constructor).toBe(Uint8Array)
    store.stop()
  })

  it('keeps refresh preference in sync (webui-ui: 刷新频率控制)', () => {
    const { store } = openStore()
    expect(store.refreshInterval).toBe(3000)
    store.setRefreshInterval(5000)
    expect(store.refreshInterval).toBe(5000)
    expect(localStorage.getItem('xkeeper.refresh_ms')).toBe('5000')
    store.stop()
  })
  it('tracks pending changes from snapshot and STATUS deltas (apply-workflow)', async () => {
    const { store, ws } = openStore()
    const withPending = doc([prog('a')])
    withPending.pending = {
      programs: [{ app: 'a', program: 'p1', running: true }],
      apps_added: ['b'],
      apps_removed: [],
      daemon_hints: [],
      errors: [],
    }
    ws.serverFrame(frameFor(MSG.SNAPSHOT, encode(withPending as never)))
    await new Promise((r) => setTimeout(r, 0))
    expect(store.pending.programs).toEqual([{ app: 'a', program: 'p1', running: true }])
    expect(store.pending.apps_added).toEqual(['b'])

    // A pending-clearing delta (post-apply) empties the projection. The
    // server sends STATUS as JSON text ({programs, pending}).
    const cleared = doc([prog('a')])
    const payload = JSON.stringify({ programs: [], pending: cleared.pending })
    ws.serverFrame(frameFor(MSG.STATUS, new TextEncoder().encode(payload) as never))
    await new Promise((r) => setTimeout(r, 0))
    expect(store.pending.programs).toEqual([])
    store.stop()
  })

  it('apply() posts the scope to /api/apply and surfaces the result', async () => {
    const { store } = openStore()
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce({
        ok: true,
        json: async () => ({ result: 'applied: 1 program change(s)' }),
      })
      .mockResolvedValueOnce({ ok: true, json: async () => doc([prog('a')]) })
    vi.stubGlobal('fetch', fetchMock)
    const msg = await store.apply({ app: 'demo', restart: true })
    expect(msg).toContain('applied')
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit]
    expect(url).toBe('/api/apply')
    expect(JSON.parse(String(init.body))).toEqual({ restart: true, app: 'demo' })
    store.stop()
  })
})
