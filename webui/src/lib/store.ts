/**
 * Console store: merges WS snapshots/deltas into one StatusDoc, owns the
 * transport lifecycle (WS preferred, periodic polling as fallback), and
 * exposes log subscriptions to viewers. Framework-agnostic — Lit
 * components bind through `consoleController`.
 *
 * Transport rules (webui-ui: 状态实时刷新 + 后端不可达降级):
 * - WS open   → live (snapshot then deltas, server-side cadence),
 * - WS down   → poll `/api/overview` at the user's refresh interval and flag
 *               the doc as polled so the UI can show a degraded banner,
 * - WS back   → stop polling; the fresh snapshot clears staleness.
 */

import { loadRefreshMs, saveRefreshMs, type RefreshMs } from './prefs.js'
import type { ProgramInfo, StatusDoc, StreamId } from './types.js'
import { WsClient, type WsState } from './ws-client.js'
import type { ReactiveController, ReactiveControllerHost } from 'lit'

export type Transport = 'connecting' | 'live' | 'poll' | 'offline'

export interface ConsoleSnapshot {
  doc: StatusDoc | null
  transport: Transport
}

export type LogListener = (frame: { program: string; stream: StreamId; text: string }) => void

function wsUrl(): string {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${proto}//${location.host}/ws`
}

export class ConsoleStore {
  private doc: StatusDoc | null = null
  private transport: Transport = 'connecting'
  private listeners = new Set<() => void>()
  private client: WsClient | null = null
  private pollTimer: ReturnType<typeof setInterval> | null = null
  private logListeners = new Set<LogListener>()
  private gapListeners = new Set<(program: string, stream: StreamId, skipped: number) => void>()
  private refreshMs: RefreshMs = loadRefreshMs()
  private logFrames: { program: string; stream: StreamId; text: string }[] = []
  private flushScheduled = false

  /** Log subscriptions survive reconnects: WS resubscribes on snapshot. */
  private subscriptions = new Set<string>() // `${program}\u0000${stream}`

  start(): void {
    if (this.client) return
    this.client = new WsClient(wsUrl(), {
      snapshot: (doc) => {
        this.doc = {
          daemon: doc.daemon,
          programs: [...doc.programs].sort(byAppThenName),
        }
        this.setTransport('live')
        this.stopPolling()
        // A fresh connection resets log routing; re-subscribe viewers.
        for (const key of this.subscriptions) {
          const idx = key.indexOf('\u0000')
          this.client?.subscribe(key.slice(0, idx), Number(key[idx + 1]) as 0 | 1)
        }
        this.emit()
      },
      delta: (programs) => {
        if (!this.doc) return
        this.mergeDelta(programs)
        this.emit()
      },
      log: (program, stream, text) => {
        this.logFrames.push({ program, stream, text })
        this.flushLogFrames()
      },
      gap: (program, stream, skipped) => {
        for (const g of this.gapListeners) g(program, stream, skipped)
      },
      error: () => {
        /* frame-level errors surface as logs; nothing actionable */
      },
      stateChange: (state: WsState) => {
        if (state === 'open') return // snapshot handler flips to live
        if (state === 'closed') {
          this.setTransport(this.doc ? 'poll' : 'offline')
          this.startPolling()
        }
      },
    })
    this.client.connect()
  }

  stop(): void {
    this.client?.close()
    this.client = null
    this.stopPolling()
  }

  // -- reactive glue --------------------------------------------------------

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener)
    return () => this.listeners.delete(listener)
  }

  getSnapshot(): ConsoleSnapshot {
    return { doc: this.doc, transport: this.transport }
  }

  get refreshInterval(): RefreshMs {
    return this.refreshMs
  }

  setRefreshInterval(ms: RefreshMs): void {
    this.refreshMs = ms
    saveRefreshMs(ms)
    // Polling cadence follows the preference; WS cadence is server-side.
    if (this.pollTimer) {
      this.stopPolling()
      this.startPolling()
    }
    this.emit()
  }

  // -- log subscriptions ------------------------------------------------------

  subscribeLogs(program: string, stream: StreamId): void {
    const key = `${program}\u0000${stream}`
    this.subscriptions.add(key)
    this.client?.subscribe(program, stream)
  }

  unsubscribeLogs(program: string, stream: StreamId): void {
    const key = `${program}\u0000${stream}`
    this.subscriptions.delete(key)
    this.client?.unsubscribe(program, stream)
  }

  onLogFrame(l: LogListener): () => void {
    this.logListeners.add(l)
    return () => this.logListeners.delete(l)
  }

  onGap(l: (program: string, stream: StreamId, skipped: number) => void): () => void {
    this.gapListeners.add(l)
    return () => this.gapListeners.delete(l)
  }

  /** Batch frames per microtask so viewers append in render-sized lumps. */
  private flushLogFrames(): void {
    if (this.flushScheduled) return
    this.flushScheduled = true
    queueMicrotask(() => {
      this.flushScheduled = false
      for (const f of this.logFrames) {
        for (const l of this.logListeners) l(f)
      }
      this.logFrames = []
    })
  }

  // -- internals ---------------------------------------------------------------

  private mergeDelta(programs: ProgramInfo[]): void {
    if (!this.doc) return
    const byName = new Map(this.doc.programs.map((p) => [p.name, p]))
    for (const p of programs) byName.set(p.name, p)
    this.doc = { daemon: this.doc.daemon, programs: [...byName.values()].sort(byAppThenName) }
  }

  private async pollOnce(): Promise<void> {
    try {
      const res = await fetch('/api/overview')
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const doc = (await res.json()) as StatusDoc
      this.doc = { daemon: doc.daemon, programs: [...doc.programs].sort(byAppThenName) }
      // Polling succeeded → the backend is reachable (degraded but alive).
      if (this.transport !== 'live') this.setTransport('poll')
      this.emit()
    } catch {
      this.setTransport('offline')
      this.emit()
    }
  }

  private startPolling(): void {
    if (this.pollTimer) return
    void this.pollOnce()
    this.pollTimer = setInterval(() => void this.pollOnce(), this.refreshMs)
  }

  private stopPolling(): void {
    if (this.pollTimer) clearInterval(this.pollTimer)
    this.pollTimer = null
  }

  private setTransport(t: Transport): void {
    if (this.transport === t) return
    this.transport = t
    this.emit()
  }

  private emit(): void {
    for (const l of this.listeners) l()
  }
}

function byAppThenName(a: ProgramInfo, b: ProgramInfo): number {
  return (a.app + '\u0000' + a.name).localeCompare(b.app + '\u0000' + b.name)
}

/** Lit reactive controller: bind a component to the console store. */
export class ConsoleController implements ReactiveController {
  private unsubscribe: (() => void) | null = null

  constructor(
    private host: ReactiveControllerHost,
    private store: ConsoleStore,
  ) {
    host.addController(this)
  }

  get snapshot(): ConsoleSnapshot {
    return this.store.getSnapshot()
  }

  hostConnected(): void {
    this.store.start()
    this.unsubscribe = this.store.subscribe(() => this.host.requestUpdate())
    this.host.requestUpdate()
  }

  hostDisconnected(): void {
    this.unsubscribe?.()
    this.unsubscribe = null
  }
}

/** Shared singleton for the app shell + views. */
export const consoleStore = new ConsoleStore()
