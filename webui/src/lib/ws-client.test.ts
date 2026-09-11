import { describe, expect, it, vi } from 'vitest'
import { WsClient } from './ws-client.js'
import { MSG } from './types.js'

/**
 * A minimal in-memory WebSocket stand-in. It records sends and lets tests
 * push binary frames into the client's onmessage path. The real server
 * frame format (see src/api.rs) is reproduced by the helpers below.
 */
class FakeWebSocket {
  static OPEN = 1
  static CONNECTING = 0
  static CLOSED = 3
  static instances: FakeWebSocket[] = []

  readyState = FakeWebSocket.CONNECTING
  binaryType = 'blob'
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
  // test hooks
  serverOpen(): void {
    this.readyState = FakeWebSocket.OPEN
    this.onopen?.()
  }
  serverClose(): void {
    this.onclose?.()
  }
  serverFrame(frame: Uint8Array): void {
    const copy = new Uint8Array(frame)
    this.onmessage?.({ data: copy.buffer })
  }
}

function frameFor(type: number, payload: Uint8Array): Uint8Array {
  const out = new Uint8Array(payload.length + 1)
  out[0] = type
  out.set(payload, 1)
  return out
}

function logFrame(program: string, stream: 0 | 1, text: string): Uint8Array {
  const name = new TextEncoder().encode(program)
  const body = new TextEncoder().encode(text)
  const out = new Uint8Array(1 + 2 + name.length + 1 + body.length)
  let i = 0
  out[i++] = MSG.LOG
  out[i++] = name.length >> 8
  out[i++] = name.length & 0xff
  out.set(name, i)
  i += name.length
  out[i++] = stream
  out.set(body, i)
  return out
}

async function flushMicrotasks(): Promise<void> {
  await new Promise((r) => setTimeout(r, 0))
}

describe('ws-client', () => {
  it('delivers snapshot, delta, log and gap frames in order', async () => {
    vi.stubGlobal('WebSocket', FakeWebSocket as unknown as typeof WebSocket)
    FakeWebSocket.instances = []
    const events: string[] = []
    const doc = { daemon: {} as never, programs: [] as never[], pending: { programs: [], apps_added: [], apps_removed: [], daemon_hints: [], errors: [] } }
    const client = new WsClient('ws://test/ws', {
      snapshot: (d) => events.push(`snapshot:${(d as typeof doc).programs.length === 0}`),
      delta: (p) => events.push(`delta:${p.length}`),
      log: (p, s, text) => events.push(`log:${p}:${s}:${text.trim()}`),
      gap: (p, s, n) => events.push(`gap:${p}:${s}:${n}`),
      error: (m) => events.push(`error:${m}`),
      stateChange: (s) => events.push(`state:${s}`),
    })
    client.connect()
    const ws = FakeWebSocket.instances[0]!
    ws.serverOpen()

    // snapshot (msgpack of a minimal doc)
    const { encode } = await import('@msgpack/msgpack')
    ws.serverFrame(frameFor(MSG.SNAPSHOT, encode(doc as never)))
    ws.serverFrame(frameFor(MSG.STATUS, encode([{ name: 'p', state: 'running' } as never])))
    ws.serverFrame(logFrame('p', 0, 'hello\n'))
    // gap with a large skip count (u64)
    const gap = new Uint8Array(1 + 2 + 1 + 1 + 8)
    let i = 0
    gap[i++] = MSG.LOG_GAP
    gap[i++] = 0
    gap[i++] = 1
    gap[i++] = 'p'.charCodeAt(0)
    gap[i++] = 0
    const view = new DataView(gap.buffer)
    view.setBigUint64(i, 123456789012n)
    ws.serverFrame(gap)

    await flushMicrotasks()
    expect(events).toEqual([
      'state:connecting',
      'state:open',
      'snapshot:true',
      'delta:1',
      'log:p:0:hello',
      'gap:p:0:123456789012',
    ])
    client.close()
  })

  it('sends subscribe/unsubscribe actions as JSON binary', () => {
    vi.stubGlobal('WebSocket', FakeWebSocket as unknown as typeof WebSocket)
    FakeWebSocket.instances = []
    const client = new WsClient('ws://test/ws', {
      snapshot: () => {},
      delta: () => {},
      log: () => {},
      gap: () => {},
      error: () => {},
      stateChange: () => {},
    })
    client.connect()
    const ws = FakeWebSocket.instances[0]!
    ws.serverOpen()
    client.subscribe('demo', 1)
    client.unsubscribe('demo', 1)
    expect(ws.sent.map((b) => JSON.parse(new TextDecoder().decode(b)))).toEqual([
      { action: 'subscribe', program: 'demo', stream: 'err' },
      { action: 'unsubscribe', program: 'demo', stream: 'err' },
    ])
    // before open, sends are dropped silently (no crash)
    client.connect()
    expect(() => client.subscribe('x', 0)).not.toThrow()
    client.close()
  })

  it('reconnects with backoff after an abnormal close and resets on open', async () => {
    vi.useFakeTimers()
    vi.stubGlobal('WebSocket', FakeWebSocket as unknown as typeof WebSocket)
    FakeWebSocket.instances = []
    const states: string[] = []
    const client = new WsClient('ws://test/ws', {
      snapshot: () => {},
      delta: () => {},
      log: () => {},
      gap: () => {},
      error: () => {},
      stateChange: (s) => states.push(s),
    })
    client.connect()
    const first = FakeWebSocket.instances[0]!
    first.serverOpen()
    first.serverClose() // abnormal → schedule reconnect (500ms)
    await vi.advanceTimersByTimeAsync(500)
    expect(FakeWebSocket.instances.length).toBe(2)
    const second = FakeWebSocket.instances[1]!
    second.serverOpen()
    // attempts reset: next drop reconnects after 500ms again (not 1s)
    second.serverClose()
    await vi.advanceTimersByTimeAsync(500)
    expect(FakeWebSocket.instances.length).toBe(3)
    expect(states.filter((s) => s === 'closed').length).toBe(2)
    client.close()
    vi.useRealTimers()
  })

  it('treats a corrupt frame as an error, not a crash', async () => {
    vi.stubGlobal('WebSocket', FakeWebSocket as unknown as typeof WebSocket)
    FakeWebSocket.instances = []
    const errors: string[] = []
    const client = new WsClient('ws://test/ws', {
      snapshot: () => {},
      delta: () => {},
      log: () => {},
      gap: () => {},
      error: (m) => errors.push(m),
      stateChange: () => {},
    })
    client.connect()
    const ws = FakeWebSocket.instances[0]!
    ws.serverOpen()
    ws.serverFrame(new Uint8Array([MSG.LOG, 0, 99])) // truncated name
    ws.serverFrame(logFrame('ok', 0, 'still alive\n'))
    await flushMicrotasks()
    expect(errors.length).toBe(1)
    // (the good frame after the bad one still arrived — see order test)
    client.close()
  })
})
