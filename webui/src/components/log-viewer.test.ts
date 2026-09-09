// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import './log-viewer.js'
import type { XkeeperLogViewer } from './log-viewer.js'
import { ConsoleStore } from '../lib/store.js'
import { MSG, type StreamId } from '../lib/types.js'

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

function logFrame(program: string, stream: StreamId, text: string): Uint8Array {
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

function gapFrame(program: string, stream: StreamId, skipped: number): Uint8Array {
  const name = new TextEncoder().encode(program)
  const out = new Uint8Array(1 + 2 + name.length + 1 + 8)
  let i = 0
  out[i++] = MSG.LOG_GAP
  out[i++] = name.length >> 8
  out[i++] = name.length & 0xff
  out.set(name, i)
  i += name.length
  out[i++] = stream
  new DataView(out.buffer).setBigUint64(i, BigInt(skipped))
  return out
}

describe('xkeeper-log-viewer', () => {
  let store: ConsoleStore
  let ws: FakeWebSocket
  let el: XkeeperLogViewer

  beforeEach(async () => {
    vi.useFakeTimers()
    vi.stubGlobal('WebSocket', FakeWebSocket as unknown as typeof WebSocket)
    FakeWebSocket.instances = []
    store = new ConsoleStore()
    store.start()
    ws = FakeWebSocket.instances[0]!
    ws.serverOpen()
    el = document.createElement('xkeeper-log-viewer') as XkeeperLogViewer
    el.store = store
    el.program = 'p'
    document.body.appendChild(el)
    await vi.advanceTimersByTimeAsync(10)
    expect(el.shadowRoot).not.toBeNull()
  })

  afterEach(() => {
    store.stop()
    el.remove()
    vi.useRealTimers()
    vi.unstubAllGlobals()
    vi.restoreAllMocks()
  })

  function rows(): (Element | null | undefined)[] {
    return [...el.shadowRoot!.querySelectorAll('.row, .gap-row')]
  }

  it('subscribes on attach and renders incoming lines', async () => {
    expect(JSON.parse(new TextDecoder().decode(ws.sent.at(-1)!))).toEqual({
      action: 'subscribe',
      program: 'p',
      stream: 'out',
    })
    ws.serverFrame(logFrame('p', 0, 'line-1\nline-2\n'))
    await vi.advanceTimersByTimeAsync(120)
    expect(rows().map((r) => r!.textContent)).toEqual(['line-1', 'line-2'])
  })

  it('renders gap markers with distinct styling, never as log lines', async () => {
    ws.serverFrame(gapFrame('p', 0, 250))
    ws.serverFrame(logFrame('p', 0, 'after\n'))
    await vi.advanceTimersByTimeAsync(120)
    const gap = el.shadowRoot!.querySelector('.gap-row')
    expect(gap).not.toBeNull()
    expect(gap!.textContent).toContain('250')
    expect(el.shadowRoot!.textContent).toContain('after')
  })

  it('stays bounded under a flood (MAX_ENTRIES)', async () => {
    for (let b = 0; b < 12; b++) {
      const lines = Array.from({ length: 500 }, (_, i) => `b${b}l${i}`).join('\n')
      ws.serverFrame(logFrame('p', 0, `${lines}\n`))
    }
    // 6000 lines pushed; render is rAF/throttle-batched.
    await vi.advanceTimersByTimeAsync(2000)
    expect(el.entryCount).toBeLessThanOrEqual(5000)
    expect(rows().length).toBeLessThanOrEqual(5000)
    // The OLDEST content is gone: b0l0 must not survive a 6000-line flood.
    const texts = rows().map((r) => r!.textContent)
    expect(texts).not.toContain('b0l0')
    expect(texts.at(-1)).toBe('b11l499')
  })

  it('switching to stderr unsubscribes stdout and re-subscribes', async () => {
    const before = ws.sent.length
    const buttons = el.shadowRoot!.querySelectorAll('.tabs button')
    ;(buttons[1] as HTMLButtonElement).click()
    await vi.advanceTimersByTimeAsync(10)
    expect(JSON.parse(new TextDecoder().decode(ws.sent.at(-2)!))).toEqual({
      action: 'unsubscribe',
      program: 'p',
      stream: 'out',
    })
    expect(JSON.parse(new TextDecoder().decode(ws.sent.at(-1)!))).toEqual({
      action: 'subscribe',
      program: 'p',
      stream: 'err',
    })
    expect(before).toBeGreaterThan(0)
  })

  it('follow pins to bottom; manual scroll-up suspends following', async () => {
    ws.serverFrame(logFrame('p', 0, Array.from({ length: 200 }, (_, i) => `l${i}`).join('\n') + '\n'))
    await vi.advanceTimersByTimeAsync(150)
    const scroller = el.shadowRoot!.querySelector('.scroller') as HTMLElement
    // happy-dom has no real layout: scrollHeight is 0, so "at bottom" holds
    // and the follow checkbox stays on after paint.
    expect(el.following).toBe(true)
    // Simulate the user scrolling up (viewport not at bottom).
    Object.defineProperty(scroller, 'scrollHeight', { value: 5000, configurable: true })
    Object.defineProperty(scroller, 'clientHeight', { value: 200, configurable: true })
    scroller.scrollTop = 100
    scroller.dispatchEvent(new Event('scroll'))
    expect(el.following).toBe(false)
    // Return to bottom → following resumes.
    scroller.scrollTop = 4800
    scroller.dispatchEvent(new Event('scroll'))
    expect(el.following).toBe(true)
  })
})
