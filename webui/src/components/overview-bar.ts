/**
 * Daemon overview bar (webui-ui: 守护进程概况): version, config source,
 * monitor interval, uptime, system CPU/memory — the page-level strip that
 * covers every app — plus the refresh-frequency selector (1/3/5/10s, default
 * 3s, persisted). The selector throttles LIST/METRIC display updates only;
 * log following is realtime regardless (webui-ui: 刷新频率控制).
 */

import { LitElement, css, html, nothing } from 'lit'
import { customElement, property, state } from 'lit/decorators.js'
import { ConsoleController, consoleStore, type ConsoleStore } from '../lib/store.js'
import type { PendingDoc } from '../lib/types.js'
import { formatBytes, formatPercent, formatUptime } from '../lib/format.js'
import { REFRESH_CHOICES_MS, loadRefreshMs, saveRefreshMs, type RefreshMs } from '../lib/prefs.js'

@customElement('xkeeper-overview-bar')
export class XkeeperOverviewBar extends LitElement {
  /** Injectable for tests; defaults to the shared singleton. */
  @property({ attribute: false }) store: ConsoleStore = consoleStore

  private console: ConsoleController | null = null

  private get consoleState() {
    return this.console?.snapshot ?? { doc: null, transport: 'connecting' as const }
  }

  @state() private refreshMs: RefreshMs = loadRefreshMs()
  @state() private applying = false
  @state() private applyResult: string | null = null
  @state() private applyError: string | null = null

  static styles = css`
    :host {
      display: block;
    }
    .pending {
      display: flex;
      align-items: center;
      flex-wrap: wrap;
      gap: var(--xkeeper-space-2) var(--xkeeper-space-3);
      padding: var(--xkeeper-space-2) var(--xkeeper-space-4);
      margin-bottom: calc(-1 * var(--xkeeper-space-1, 4px));
      background: color-mix(in srgb, var(--xkeeper-accent-strong, #6366f1) 12%, var(--xkeeper-surface));
      border: 1px solid color-mix(in srgb, var(--xkeeper-accent-strong, #6366f1) 35%, var(--xkeeper-border));
      border-radius: var(--xkeeper-radius-lg, 10px);
      font-size: 0.82rem;
      color: var(--xkeeper-text);
    }
    .pending .badge {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      font-weight: 600;
      color: var(--xkeeper-text-bright);
    }
    .pending .count {
      display: inline-grid;
      place-items: center;
      min-width: 18px;
      height: 18px;
      padding: 0 5px;
      border-radius: 9px;
      background: var(--xkeeper-accent-strong, #6366f1);
      color: var(--xkeeper-accent-ink, #fff);
      font-size: 0.72rem;
      font-weight: 700;
    }
    .pending .detail {
      color: var(--xkeeper-text-dim);
      max-width: 46ch;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .pending .actions {
      margin-left: auto;
      display: inline-flex;
      align-items: center;
      gap: 8px;
    }
    .pending .restart-opt {
      display: inline-flex;
      align-items: center;
      gap: 4px;
      color: var(--xkeeper-text-dim);
      font-size: 0.78rem;
      cursor: pointer;
    }
    .pending button {
      border: 1px solid var(--xkeeper-border);
      background: var(--xkeeper-surface-2);
      color: var(--xkeeper-text);
      font: inherit;
      padding: 4px 12px;
      border-radius: var(--xkeeper-radius-sm, 6px);
      cursor: pointer;
    }
    .pending button[data-primary='true'] {
      background: var(--xkeeper-accent-strong, #6366f1);
      border-color: transparent;
      color: var(--xkeeper-accent-ink, #fff);
      font-weight: 600;
    }
    .pending button:disabled {
      opacity: 0.55;
      cursor: default;
    }
    .apply-result {
      margin-top: 4px;
      padding: var(--xkeeper-space-2) var(--xkeeper-space-3);
      font-size: 0.8rem;
      white-space: pre-wrap;
      background: var(--xkeeper-surface);
      border: 1px solid var(--xkeeper-border);
      border-radius: var(--xkeeper-radius-lg, 10px);
      color: var(--xkeeper-text-dim);
      max-height: 12em;
      overflow-y: auto;
    }
    .apply-result[data-error='true'] {
      border-color: var(--xkeeper-status-failed, #ef4444);
      color: var(--xkeeper-text);
    }
    .bar {
      display: flex;
      flex-wrap: wrap;
      align-items: center;
      gap: var(--xkeeper-space-3) var(--xkeeper-space-5);
      padding: var(--xkeeper-space-2) var(--xkeeper-space-4);
      background: var(--xkeeper-surface);
      border: 1px solid var(--xkeeper-border);
      border-radius: var(--xkeeper-radius-lg, 10px);
      font-size: 0.82rem;
      color: var(--xkeeper-text-dim);
    }
    .item {
      display: inline-flex;
      align-items: baseline;
      gap: 6px;
      white-space: nowrap;
    }
    .item .k {
      font-size: 0.72rem;
      letter-spacing: 0.06em;
      text-transform: uppercase;
      color: var(--xkeeper-text-faint);
    }
    .item .v {
      color: var(--xkeeper-text);
      font-variant-numeric: tabular-nums;
    }
    .cpu,
    .mem {
      min-width: 150px;
    }
    .meter {
      position: relative;
      flex: 1 1 60px;
      height: 4px;
      min-width: 48px;
      align-self: center;
      border-radius: 2px;
      background: var(--xkeeper-surface-3);
      overflow: hidden;
    }
    .meter > span {
      position: absolute;
      inset: 0 auto 0 0;
      border-radius: 2px;
      background: var(--xkeeper-accent-strong);
      transition: width 0.4s var(--xkeeper-ease, ease);
    }
    .meter > span[data-hot='true'] {
      background: var(--xkeeper-signal);
    }
    .refresh {
      margin-left: auto;
      display: inline-flex;
      align-items: center;
      gap: 6px;
    }
    .refresh select {
      background: var(--xkeeper-surface-2);
      color: var(--xkeeper-text);
      border: 1px solid var(--xkeeper-border);
      border-radius: var(--xkeeper-radius-sm, 6px);
      font: inherit;
      padding: 3px 6px;
    }
    .refresh label {
      font-size: 0.72rem;
      letter-spacing: 0.06em;
      text-transform: uppercase;
      color: var(--xkeeper-text-faint);
    }
  `

  connectedCallback(): void {
    super.connectedCallback()
    this.console = new ConsoleController(this, this.store)
  }

  private onApply(): void {
    if (this.applying) return
    const detail = this.pendingSummary(this.consoleState.doc?.pending)
    if (!window.confirm(
      `应用全部待应用变更？\n${detail || '（无）'}\n变更的程序将被停止并以新定义重启（原先停止的保持停止）。`,
    )) {
      return
    }
    this.applying = true
    this.applyError = null
    this.applyResult = null
    this.store
      .apply({})
      .then((r) => {
        this.applyResult = r
      })
      .catch((e: unknown) => {
        this.applyError = String(e instanceof Error ? e.message : e)
      })
      .finally(() => {
        this.applying = false
      })
  }

  private pendingOneLiner(p: PendingDoc | undefined): string {
    if (!p) return ''
    const parts: string[] = []
    if (p.programs.length) parts.push(`${p.programs.length} 个程序配置有变化`)
    if (p.apps_added.length) parts.push(`新增 app: ${p.apps_added.join(', ')}`)
    if (p.apps_removed.length) parts.push(`注销 app: ${p.apps_removed.join(', ')}`)
    if (p.daemon_hints.length) parts.push(p.daemon_hints[0]!)
    if (p.errors.length) parts.push(`${p.errors.length} 个应用检出失败`)
    return parts.join(' · ')
  }

  private pendingSummary(p: PendingDoc | undefined): string {
    if (!p) return ''
    const lines: string[] = []
    for (const x of p.programs.slice(0, 20)) {
      lines.push(`  ${x.app}.${x.program} [${x.running ? 'running' : 'stopped'}] 配置有变化`)
    }
    if (p.programs.length > 20) lines.push(`  … 共 ${p.programs.length} 个`)
    for (const a of p.apps_added) lines.push(`  app[${a}] 新注册`)
    for (const a of p.apps_removed) lines.push(`  app[${a}] 已注销`)
    for (const h of p.daemon_hints) lines.push(`  ${h}`)
    for (const e of p.errors) lines.push(`  ${e}`)
    return lines.join('\n')
  }

  private onRefreshChange(e: Event): void {
    const value = Number((e.target as HTMLSelectElement).value) as RefreshMs
    this.refreshMs = value
    saveRefreshMs(value)
    this.store.setRefreshInterval(value)
  }

  render() {
    const { doc } = this.consoleState
    const daemon = doc?.daemon
    const sys = daemon?.system
    const memPct =
      sys && sys.mem_total_bytes > 0
        ? Math.min(100, (sys.mem_used_bytes / sys.mem_total_bytes) * 100)
        : null
    const pending = this.consoleState.doc?.pending
    const pendingCount =
      (pending?.programs.length ?? 0) +
      (pending?.apps_added.length ?? 0) +
      (pending?.apps_removed.length ?? 0) +
      (pending?.daemon_hints.length ?? 0)
    return html`
      ${pendingCount > 0
        ? html`<div class="pending" role="alert" aria-label="待应用变更">
            <span class="badge">待应用变更 <span class="count">${pendingCount}</span></span>
            <span class="detail">${this.pendingOneLiner(pending)}</span>
            <span class="actions">
              <button
                data-primary="true"
                ?disabled=${this.applying}
                @click=${this.onApply}
                title="应用全部待应用变更（变更程序停止后重建重启，手动停止的保持停止）"
              >
                ${this.applying ? '应用中…' : 'Apply 全部'}
              </button>
            </span>
          </div>`
        : nothing}
      ${this.applyResult || this.applyError
        ? html`<div
            class="pending"
            role=${this.applyError ? 'alert' : 'status'}
            aria-label="apply 结果"
          >
            ${this.applyResult
              ? html`<span class="detail">${this.applyResult}</span>`
              : nothing}
            ${this.applyError
              ? html`<span class="detail" data-error="true">${this.applyError}</span>`
              : nothing}
          </div>`
        : nothing}
      <div class="bar" role="status" aria-label="守护进程概况">
        <span class="item"><span class="k">版本</span><span class="v">${daemon?.version ?? '—'}</span></span>
        <span class="item" title=${daemon?.config_source ?? ''}>
          <span class="k">配置</span>
          <span class="v">${daemon?.config_source ?? '—'}</span>
        </span>
        <span class="item"><span class="k">巡检</span><span class="v">${daemon ? `${daemon.monitor_interval}s` : '—'}</span></span>
        <span class="item"><span class="k">运行</span><span class="v">${formatUptime(daemon?.uptime_secs)}</span></span>
        <span class="item cpu">
          <span class="k">系统 CPU</span>
          <span class="v">${formatPercent(sys?.cpu_percent ?? null)}</span>
          <span class="meter" role="presentation"
            ><span data-hot=${(sys?.cpu_percent ?? 0) > 75} style=${`width:${sys?.cpu_percent ?? 0}%`}></span
          ></span>
        </span>
        <span class="item mem">
          <span class="k">系统内存</span>
          <span class="v">
            ${sys ? `${formatBytes(sys.mem_used_bytes)} / ${formatBytes(sys.mem_total_bytes)}` : '—'}
          </span>
          <span class="meter" role="presentation"
            ><span data-hot=${(memPct ?? 0) > 90} style=${`width:${memPct ?? 0}%`}></span
          ></span>
        </span>
        <span class="refresh">
          <label for="refresh-select">刷新</label>
          <select id="refresh-select" .value=${String(this.refreshMs)} @change=${this.onRefreshChange}>
            ${REFRESH_CHOICES_MS.map(
              (ms) => html`<option value=${ms} ?selected=${ms === this.refreshMs}>${ms / 1000}s</option>`,
            )}
          </select>
        </span>
      </div>
    `
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'xkeeper-overview-bar': XkeeperOverviewBar
  }
}
