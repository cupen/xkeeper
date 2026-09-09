/**
 * Daemon overview bar (webui-ui: 守护进程概况): version, config source,
 * monitor interval, uptime, system CPU/memory — the page-level strip that
 * covers every app — plus the refresh-frequency selector (1/3/5/10s, default
 * 3s, persisted). The selector throttles LIST/METRIC display updates only;
 * log following is realtime regardless (webui-ui: 刷新频率控制).
 */

import { LitElement, css, html } from 'lit'
import { customElement, property, state } from 'lit/decorators.js'
import { ConsoleController, consoleStore, type ConsoleStore } from '../lib/store.js'
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

  static styles = css`
    :host {
      display: block;
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
    return html`
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
