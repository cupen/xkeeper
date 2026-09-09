/**
 * App overview (webui-ui: App 概况): the app's program summary table —
 * name, status, restarts, uptime, CPU, memory, log line rate — plus the
 * rate-window switch (1s/10s/1m/5m, shared by every rate column) and links
 * into each program's detail. Not-running programs show placeholders, never
 * fake zeros; fatal rows are highlighted.
 */

import { LitElement, css, html } from 'lit'
import { customElement, property, state } from 'lit/decorators.js'
import { nothing } from 'lit'
import { ConsoleController, consoleStore } from '../lib/store.js'
import type { ProgramInfo } from '../lib/types.js'
import {
  formatBytes,
  formatPercent,
  formatRate,
  formatUptime,
  formatWindow,
  stateGlyph,
} from '../lib/format.js'
import {
  RATE_WINDOW_CHOICES_S,
  loadRateWindowS,
  saveRateWindowS,
  type RateWindowS,
} from '../lib/prefs.js'
import { navigate } from '../router.js'

@customElement('xkeeper-app-overview')
export class XkeeperAppOverview extends LitElement {
  @property() app = ''

  private console = new ConsoleController(this, consoleStore)

  @state() private rateWindow: RateWindowS = loadRateWindowS()

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
    }
    h1 {
      margin: 0;
      font-size: 1.25rem;
      color: var(--xkeeper-text-bright);
    }
    .sub {
      color: var(--xkeeper-text-faint);
      font-size: 0.85rem;
    }
    .window-switch {
      margin-left: auto;
      display: inline-flex;
      align-items: center;
      gap: 6px;
      font-size: 0.8rem;
      color: var(--xkeeper-text-dim);
    }
    .window-switch select {
      background: var(--xkeeper-surface-2);
      color: var(--xkeeper-text);
      border: 1px solid var(--xkeeper-border);
      border-radius: var(--xkeeper-radius-sm, 6px);
      font: inherit;
      padding: 3px 6px;
    }
    table {
      width: 100%;
      border-collapse: collapse;
      background: var(--xkeeper-surface);
      border: 1px solid var(--xkeeper-border);
      border-radius: var(--xkeeper-radius-lg, 10px);
      overflow: hidden;
      font-size: 0.86rem;
    }
    th,
    td {
      text-align: left;
      padding: 9px 14px;
      border-bottom: 1px solid var(--xkeeper-border);
      font-variant-numeric: tabular-nums;
      white-space: nowrap;
    }
    th {
      font-size: 0.72rem;
      letter-spacing: 0.06em;
      text-transform: uppercase;
      color: var(--xkeeper-text-faint);
      font-weight: 600;
      background: var(--xkeeper-surface-2);
    }
    tr:last-child td {
      border-bottom: none;
    }
    tbody tr {
      cursor: pointer;
      transition: background var(--xkeeper-dur, 150ms) var(--xkeeper-ease, ease);
    }
    tbody tr:hover {
      background: var(--xkeeper-surface-2);
    }
    tr[data-fatal='true'] {
      background: var(--xkeeper-status-failed-bg);
      box-shadow: inset 3px 0 0 var(--xkeeper-status-failed);
    }
    tr[data-fatal='true']:hover {
      background: color-mix(in srgb, var(--xkeeper-status-failed-bg) 60%, var(--xkeeper-surface-2));
    }
    td.num {
      text-align: right;
    }
    .name-cell {
      display: inline-flex;
      align-items: center;
      gap: 8px;
      color: var(--xkeeper-text-bright);
      font-weight: 600;
    }
    .name-cell .glyph {
      color: var(--xkeeper-text-faint);
    }
    .prog-count {
      color: var(--xkeeper-text-faint);
      font-size: 0.9rem;
    }
    .empty {
      padding: var(--xkeeper-space-6);
      text-align: center;
      color: var(--xkeeper-text-faint);
    }
  `

  private programsOf(app: string): ProgramInfo[] {
    return this.console.snapshot.doc?.programs.filter((p) => p.app === app) ?? []
  }

  private rateOf(p: ProgramInfo, stream: 'out' | 'err'): number {
    const w = this.rateWindow
    const r = p.log_rate[stream]
    switch (w) {
      case 1:
        return r.w1
      case 60:
        return r.w60
      case 300:
        return r.w300
      default:
        return r.w10
    }
  }

  private onWindowChange(e: Event): void {
    this.rateWindow = Number((e.target as HTMLSelectElement).value) as RateWindowS
    saveRateWindowS(this.rateWindow)
  }

  private openDetail(program: string): void {
    navigate(`/app/${encodeURIComponent(this.app)}/program/${encodeURIComponent(program)}`)
  }

  render() {
    const programs = this.programsOf(this.app)
    return html`
      <header>
        <h1>▤ ${this.app}</h1>
        <span class="sub">${programs.length} 个程序</span>
        <label class="window-switch">
          速率窗口
          <select .value=${String(this.rateWindow)} @change=${this.onWindowChange}>
            ${RATE_WINDOW_CHOICES_S.map(
              (s) => html`<option value=${s} ?selected=${s === this.rateWindow}>${formatWindow(s)}</option>`,
            )}
          </select>
        </label>
      </header>
      ${programs.length === 0
        ? html`<div class="empty">该应用没有声明任何程序</div>`
        : html`
            <table>
              <thead>
                <tr>
                  <th>程序</th>
                  <th>状态</th>
                  <th class="num">重启</th>
                  <th class="num">uptime</th>
                  <th class="num">CPU</th>
                  <th class="num">内存</th>
                  <th class="num">stdout 速率</th>
                  <th class="num">stderr 速率</th>
                </tr>
              </thead>
              <tbody>
                ${programs.map(
                  (p) => html`
                    <tr
                      data-fatal=${p.state === 'fatal'}
                      @click=${() => this.openDetail(p.name)}
                      title="查看 ${p.name} 详情"
                    >
                      <td><span class="name-cell"><span class="glyph" aria-hidden="true">${stateGlyph(p.state)}</span>${p.name}</span></td>
                      <td>
                        <xkeeper-status-badge slug=${p.state} label=${p.state} glyph=${stateGlyph(p.state)}
                        ></xkeeper-status-badge>
                        ${p.wait_reason ? html`<span class="sub">　${p.wait_reason}</span>` : nothing}
                        ${p.fatal_reason ? html`<span class="sub">　${p.fatal_reason}</span>` : nothing}
                      </td>
                      <td class="num">${p.total_exits}</td>
                      <td class="num">${formatUptime(p.uptime_secs)}</td>
                      <td class="num">${formatPercent(p.cpu_percent)}</td>
                      <td class="num">${formatBytes(p.mem_bytes)}</td>
                      <td class="num">${formatRate(this.rateOf(p, 'out'))}</td>
                      <td class="num">${formatRate(this.rateOf(p, 'err'))}</td>
                    </tr>
                  `,
                )}
              </tbody>
            </table>
          `}
    `
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'xkeeper-app-overview': XkeeperAppOverview
  }
}
