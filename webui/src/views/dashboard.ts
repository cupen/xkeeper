/**
 * Dashboard (route `/`): daemon-level overview — system resources live in
 * the overview bar; this view lists every registered app with its program
 * states, so a fatal anywhere is visible without drilling in. Replaces the
 * earlier under-construction placeholder.
 */

import { LitElement, css, html, nothing } from 'lit'
import { customElement, property } from 'lit/decorators.js'
import { ConsoleController, consoleStore, type ConsoleStore } from '../lib/store.js'
import type { ProgramInfo } from '../lib/types.js'
import { stateGlyph } from '../lib/format.js'
import { navigate } from '../router.js'

@customElement('xkeeper-dashboard')
export class XkeeperDashboard extends LitElement {
  /** Injectable for tests; defaults to the shared singleton. */
  @property({ attribute: false }) store: ConsoleStore = consoleStore

  private console: ConsoleController | null = null

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
    h1 {
      margin: 0;
      font-size: 1.25rem;
      color: var(--xkeeper-text-bright);
    }
    .banner {
      padding: var(--xkeeper-space-2) var(--xkeeper-space-3);
      border-radius: var(--xkeeper-radius-sm, 6px);
      font-size: 0.85rem;
    }
    .banner[data-mode='poll'] {
      background: color-mix(in srgb, var(--xkeeper-status-working) 12%, transparent);
      color: var(--xkeeper-status-working);
      border: 1px solid var(--xkeeper-status-working-border, transparent);
    }
    .banner[data-mode='offline'] {
      background: var(--xkeeper-status-failed-bg);
      color: var(--xkeeper-status-failed);
      border: 1px solid var(--xkeeper-status-failed-border);
    }
    .app {
      background: var(--xkeeper-surface);
      border: 1px solid var(--xkeeper-border);
      border-radius: var(--xkeeper-radius-lg, 10px);
      padding: var(--xkeeper-space-3) var(--xkeeper-space-4);
    }
    .app h2 {
      margin: 0 0 var(--xkeeper-space-2);
      font-size: 1rem;
      color: var(--xkeeper-text-bright);
    }
    .app a {
      color: inherit;
      text-decoration: none;
    }
    .app a:hover h2 {
      color: var(--xkeeper-accent-hover);
    }
    ul {
      list-style: none;
      margin: 0;
      padding: 0;
      display: flex;
      flex-direction: column;
      gap: 4px;
    }
    li {
      display: flex;
      align-items: center;
      gap: 8px;
      font-size: 0.88rem;
      color: var(--xkeeper-text-dim);
      font-variant-numeric: tabular-nums;
      cursor: pointer;
    }
    li[data-state='fatal'] {
      color: var(--xkeeper-status-failed);
    }
    li .glyph {
      width: 1em;
      text-align: center;
    }
    .fatal-note {
      color: var(--xkeeper-text-faint);
      font-size: 0.8rem;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .empty {
      padding: var(--xkeeper-space-6);
      text-align: center;
      color: var(--xkeeper-text-faint);
    }
    .empty code {
      font-family: var(--xkeeper-font-mono, monospace);
      color: var(--xkeeper-text-dim);
    }
  `

  connectedCallback(): void {
    super.connectedCallback()
    this.console = new ConsoleController(this, this.store)
  }

  private get consoleState() {
    return this.console?.snapshot ?? { doc: null, transport: 'connecting' as const }
  }

  private byApp(): Map<string, ProgramInfo[]> {
    const map = new Map<string, ProgramInfo[]>()
    for (const p of this.consoleState.doc?.programs ?? []) {
      const list = map.get(p.app) ?? []
      list.push(p)
      map.set(p.app, list)
    }
    return map
  }

  private go(e: MouseEvent, path: string): void {
    if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey || e.button !== 0) return
    e.preventDefault()
    navigate(path)
  }

  render() {
    const { doc, transport } = this.consoleState
    return html`
      <h1>总览</h1>
      ${transport === 'poll'
        ? html`<div class="banner" data-mode="poll" role="status">
            实时连接已断开 — 正以轮询方式刷新（数据可能略有延迟）
          </div>`
        : nothing}
      ${transport === 'offline'
        ? html`<div class="banner" data-mode="offline" role="alert">
            后端不可达 — 显示的是最后已知数据（已过期）；恢复后将自动更新
          </div>`
        : nothing}
      ${doc && doc.programs.length === 0
        ? html`<div class="empty">
              还没有应用。<code>xkeeper add &lt;目录&gt;</code> 注册后出现在这里。
            </div>`
        : nothing}
      ${[...this.byApp()].map(
        ([app, programs]) => html`
          <section class="app">
            <a
              href=${`/app/${encodeURIComponent(app)}`}
              @click=${(e: MouseEvent) => this.go(e, `/app/${encodeURIComponent(app)}`)}
            >
              <h2>▤ ${app}</h2>
            </a>
            <ul>
              ${programs.map(
                (p) => html`
                  <li
                    data-state=${p.state}
                    @click=${() =>
                      navigate(`/app/${encodeURIComponent(app)}/program/${encodeURIComponent(p.name)}`)}
                  >
                    <span class="glyph" aria-hidden="true">${stateGlyph(p.state)}</span>
                    <xkeeper-status-badge slug=${p.state} label=${p.state} glyph=${stateGlyph(p.state)}
                    ></xkeeper-status-badge>
                    <span>${p.name}</span>
                    ${p.fatal_reason ? html`<span class="fatal-note">${p.fatal_reason}</span>` : nothing}
                  </li>
                `,
              )}
            </ul>
          </section>
        `,
      )}
    `
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'xkeeper-dashboard': XkeeperDashboard
  }
}
