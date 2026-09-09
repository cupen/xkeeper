/**
 * Program detail (webui-ui: 进程详情 + 单程序控制): run info (command line,
 * workdir, pid, state, uptime, restarts), start/stop/restart with a confirm
 * step for destructive actions and visible success/failure feedback, and
 * the embedded log viewers.
 */

import { LitElement, css, html, nothing } from 'lit'
import { customElement, property, state } from 'lit/decorators.js'
import { ConsoleController, consoleStore, type ConsoleStore } from '../lib/store.js'
import { formatBytes, formatPercent, formatUptime, stateGlyph } from '../lib/format.js'

type PendingAction = 'stop' | 'restart' | null

@customElement('xkeeper-program-detail')
export class XkeeperProgramDetail extends LitElement {
  @property() app = ''
  @property() program = ''

  /** Injectable for tests; defaults to the shared singleton. */
  @property({ attribute: false }) store: ConsoleStore = consoleStore

  private console: ConsoleController | null = null

  private get consoleState() {
    return this.console?.snapshot ?? { doc: null, transport: 'connecting' as const }
  }

  @state() private confirm: PendingAction = null
  @state() private notice: { ok: boolean; text: string } | null = null
  @state() private busy = false

  static styles = css`
    :host {
      flex: 1;
      min-height: 0;
      overflow-y: auto;
      display: flex;
      flex-direction: column;
      gap: var(--xkeeper-space-4);
      padding: var(--xkeeper-space-4) var(--xkeeper-space-5);
      box-sizing: border-box;
    }
    header {
      display: flex;
      align-items: baseline;
      gap: var(--xkeeper-space-3);
      flex-wrap: wrap;
    }
    h1 {
      margin: 0;
      font-size: 1.2rem;
      color: var(--xkeeper-text-bright);
    }
    .crumb {
      color: var(--xkeeper-text-faint);
      font-size: 0.85rem;
    }
    .actions {
      margin-left: auto;
      display: inline-flex;
      gap: var(--xkeeper-space-2);
    }
    button {
      font: inherit;
      font-size: 0.84rem;
      padding: 6px 14px;
      border-radius: var(--xkeeper-radius-sm, 6px);
      border: 1px solid var(--xkeeper-border);
      background: var(--xkeeper-surface-2);
      color: var(--xkeeper-text);
      cursor: pointer;
      transition:
        background var(--xkeeper-dur, 150ms) var(--xkeeper-ease, ease),
        border-color var(--xkeeper-dur, 150ms) var(--xkeeper-ease, ease);
    }
    button:hover:not(:disabled) {
      border-color: var(--xkeeper-border-strong);
      background: var(--xkeeper-surface-3);
    }
    button:disabled {
      opacity: 0.5;
      cursor: not-allowed;
    }
    button[data-kind='start'] {
      border-color: var(--xkeeper-status-done-border);
      color: var(--xkeeper-status-done);
    }
    button[data-kind='stop'] {
      border-color: var(--xkeeper-status-failed-border);
      color: var(--xkeeper-status-failed);
    }
    .confirm {
      display: inline-flex;
      align-items: center;
      gap: var(--xkeeper-space-2);
      padding: 6px 12px;
      border: 1px solid var(--xkeeper-signal);
      border-radius: var(--xkeeper-radius-sm, 6px);
      background: color-mix(in srgb, var(--xkeeper-signal) 10%, transparent);
      color: var(--xkeeper-text);
      font-size: 0.84rem;
    }
    .notice {
      padding: var(--xkeeper-space-2) var(--xkeeper-space-3);
      border-radius: var(--xkeeper-radius-sm, 6px);
      font-size: 0.85rem;
    }
    .notice[data-ok='true'] {
      background: var(--xkeeper-status-done-bg);
      color: var(--xkeeper-status-done);
    }
    .notice[data-ok='false'] {
      background: var(--xkeeper-status-failed-bg);
      color: var(--xkeeper-status-failed);
    }
    .grid {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
      gap: var(--xkeeper-space-3);
    }
    .card {
      background: var(--xkeeper-surface);
      border: 1px solid var(--xkeeper-border);
      border-radius: var(--xkeeper-radius-lg, 10px);
      padding: var(--xkeeper-space-3) var(--xkeeper-space-4);
    }
    .card .k {
      display: block;
      font-size: 0.7rem;
      letter-spacing: 0.06em;
      text-transform: uppercase;
      color: var(--xkeeper-text-faint);
      margin-bottom: 4px;
    }
    .card .v {
      font-size: 0.95rem;
      color: var(--xkeeper-text-bright);
      font-variant-numeric: tabular-nums;
      word-break: break-all;
    }
    .cmd {
      grid-column: 1 / -1;
    }
    .cmd .v {
      font-family: var(--xkeeper-font-mono, monospace);
      font-size: 0.85rem;
    }
    xkeeper-log-viewer {
      min-height: 260px;
    }
    .log-title {
      margin: var(--xkeeper-space-2) 0 0;
      font-size: 0.95rem;
      color: var(--xkeeper-text-bright);
    }
    .missing {
      padding: var(--xkeeper-space-6);
      text-align: center;
      color: var(--xkeeper-text-faint);
    }
  `

  connectedCallback(): void {
    super.connectedCallback()
    this.console = new ConsoleController(this, this.store)
  }

  private get info() {
    return this.consoleState.doc?.programs.find((p) => p.app === this.app && p.name === this.program) ?? null
  }

  private async act(action: 'start' | 'stop' | 'restart'): Promise<void> {
    this.busy = true
    this.confirm = null
    this.notice = null
    try {
      const res = await fetch(`/api/programs/${encodeURIComponent(this.program)}/${action}`, {
        method: 'POST',
      })
      const body = (await res.json().catch(() => ({}))) as { result?: string; error?: string }
      if (res.ok) {
        this.notice = { ok: true, text: body.result ?? `${action} 已提交` }
      } else {
        this.notice = { ok: false, text: body.error ?? `${action} 失败（HTTP ${res.status}）` }
      }
    } catch (e) {
      this.notice = { ok: false, text: `${action} 失败：${String(e)}` }
    }
    this.busy = false
  }

  private renderActions() {
    const p = this.info
    if (!p) return nothing
    const running = p.state === 'running' || p.state === 'starting'
    const startable = !running && p.state !== 'fatal' ? true : p.state === 'fatal'
    if (this.confirm) {
      return html`
        <span class="confirm" role="alertdialog" aria-label="确认操作">
          确认对 <strong>${p.name}</strong> 执行 ${this.confirm === 'stop' ? '停止' : '重启'}？
          <button
            data-kind=${this.confirm}
            ?disabled=${this.busy}
            @click=${() => this.act(this.confirm!)}
          >
            确认
          </button>
          <button @click=${() => (this.confirm = null)}>取消</button>
        </span>
      `
    }
    return html`
      <span class="actions">
        ${startable
          ? html`<button data-kind="start" ?disabled=${this.busy} @click=${() => this.act('start')}>
              启动
            </button>`
          : nothing}
        ${running
          ? html`
              <button data-kind="stop" ?disabled=${this.busy} @click=${() => (this.confirm = 'stop')}>
                停止
              </button>
              <button ?disabled=${this.busy} @click=${() => (this.confirm = 'restart')}>重启</button>
            `
          : nothing}
      </span>
    `
  }

  render() {
    const p = this.info
    if (!p) {
      return html`<div class="missing">程序 ${this.program} 不存在（可能已被移除）</div>`
    }
    const command = [p.command, ...p.args].filter(Boolean).join(' ')
    return html`
      <header>
        <span class="crumb">${this.app} /</span>
        <h1>
          <span aria-hidden="true">${stateGlyph(p.state)}</span> ${p.name}
        </h1>
        <xkeeper-status-badge slug=${p.state} label=${p.state} glyph=${stateGlyph(p.state)}></xkeeper-status-badge>
        ${this.renderActions()}
      </header>
      ${this.notice
        ? html`<div class="notice" data-ok=${this.notice.ok} role="status">${this.notice.text}</div>`
        : nothing}
      <div class="grid">
        <div class="card cmd">
          <span class="k">命令</span>
          <span class="v">${command || '—'}</span>
        </div>
        <div class="card cmd">
          <span class="k">工作目录</span>
          <span class="v">${p.work_dir || '（继承守护进程）'}</span>
        </div>
        <div class="card">
          <span class="k">PID</span>
          <span class="v">${p.pid ?? '—'}</span>
        </div>
        <div class="card">
          <span class="k">uptime</span>
          <span class="v">${formatUptime(p.uptime_secs)}</span>
        </div>
        <div class="card">
          <span class="k">退出 / 重启</span>
          <span class="v">${p.total_exits}</span>
        </div>
        <div class="card">
          <span class="k">CPU</span>
          <span class="v">${formatPercent(p.cpu_percent)}</span>
        </div>
        <div class="card">
          <span class="k">内存</span>
          <span class="v">${formatBytes(p.mem_bytes)}</span>
        </div>
      </div>
      ${p.fatal_reason ? html`<div class="notice" data-ok="false">fatal：${p.fatal_reason}</div>` : nothing}
      ${p.last_exit ? html`<div class="notice" data-ok="false">上次退出：${p.last_exit}</div>` : nothing}

      <h2 class="log-title">日志</h2>
      <xkeeper-log-viewer .program=${this.program}></xkeeper-log-viewer>
    `
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'xkeeper-program-detail': XkeeperProgramDetail
  }
}
