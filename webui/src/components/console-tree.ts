/**
 * Sidebar two-level tree (webui-ui: 左侧两层导航): registered apps at the
 * top level, their declared programs below — including programs that never
 * started. The selected node is highlighted; selection navigates via the
 * SPA router, so deep links and browser history work for free.
 */

import { LitElement, css, html, nothing } from 'lit'
import { customElement, property } from 'lit/decorators.js'
import { navigate } from '../router.js'
import type { ProgramInfo } from '../lib/types.js'
import { stateGlyph } from '../lib/format.js'

export interface TreeApp {
  name: string
  programs: ProgramInfo[]
}

@customElement('xkeeper-console-tree')
export class XkeeperConsoleTree extends LitElement {
  @property({ type: Array }) apps: TreeApp[] = []
  /** Currently selected path segments (raw, still encoded). */
  @property() selectedApp = ''
  @property() selectedProgram = ''

  static styles = css`
    :host {
      display: block;
    }
    .app {
      margin: 0 0 var(--xkeeper-space-2);
    }
    .app-link {
      display: flex;
      align-items: center;
      gap: 8px;
      padding: 7px 10px;
      border-radius: var(--xkeeper-radius-md);
      text-decoration: none;
      color: var(--xkeeper-text-bright);
      font-weight: 600;
      font-size: 0.92rem;
    }
    .app-link:hover {
      background: var(--xkeeper-surface-2);
    }
    .app-link[data-active='true'] {
      background: var(--xkeeper-accent-soft);
      color: var(--xkeeper-accent-hover);
    }
    .app-link .count {
      margin-left: auto;
      font-size: 0.72rem;
      font-weight: 500;
      color: var(--xkeeper-text-faint);
      font-variant-numeric: tabular-nums;
    }
    .progs {
      margin: 2px 0 0 14px;
      padding-left: 10px;
      border-left: 1px solid var(--xkeeper-border);
      display: flex;
      flex-direction: column;
      gap: 1px;
    }
    a.prog {
      display: flex;
      align-items: center;
      gap: 7px;
      padding: 4px 9px;
      border-radius: var(--xkeeper-radius-sm, 6px);
      text-decoration: none;
      color: var(--xkeeper-text-dim);
      font-size: 0.86rem;
      font-variant-numeric: tabular-nums;
    }
    a.prog:hover {
      background: var(--xkeeper-surface-2);
      color: var(--xkeeper-text);
    }
    a.prog[data-active='true'] {
      background: var(--xkeeper-accent-soft);
      color: var(--xkeeper-accent-hover);
    }
    a.prog .glyph {
      width: 1em;
      text-align: center;
      flex: 0 0 auto;
    }
    a.prog .name {
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    a.prog[data-state='fatal'] {
      color: var(--xkeeper-status-failed);
    }
    a.prog[data-state='backoff'] {
      color: var(--xkeeper-status-working);
    }
    a.prog[data-state='running'] {
      color: var(--xkeeper-status-done);
    }
    .empty {
      padding: 10px;
      color: var(--xkeeper-text-faint);
      font-size: 0.85rem;
    }
  `

  private go(e: MouseEvent, path: string): void {
    if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey || e.button !== 0) return
    e.preventDefault()
    navigate(path)
  }

  render() {
    if (this.apps.length === 0) {
      return html`<div class="empty">没有已注册的应用（xkeeper add 注册后出现在这里）</div>`
    }
    return html`
      ${this.apps.map(
        (app) => html`
          <section class="app">
            <a
              class="app-link"
              href=${`/app/${encodeURIComponent(app.name)}`}
              data-active=${app.name === this.selectedApp}
              @click=${(e: MouseEvent) => this.go(e, `/app/${encodeURIComponent(app.name)}`)}
            >
              <span aria-hidden="true">▤</span>
              <span>${app.name}</span>
              <span class="count">${app.programs.length}</span>
            </a>
            ${app.programs.length > 0
              ? html`<div class="progs" role="group" aria-label=${`${app.name} 的程序`}>
                  ${app.programs.map(
                    (p) => html`
                      <a
                        class="prog"
                        data-state=${p.state}
                        data-active=${p.name === this.selectedProgram && app.name === this.selectedApp}
                        href=${`/app/${encodeURIComponent(app.name)}/program/${encodeURIComponent(p.name)}`}
                        @click=${(
                          e: MouseEvent,
                        ) => this.go(e, `/app/${encodeURIComponent(app.name)}/program/${encodeURIComponent(p.name)}`)}
                        title=${`${p.name} — ${p.state}`}
                      >
                        <span class="glyph" aria-hidden="true">${stateGlyph(p.state)}</span>
                        <span class="name">${p.name}</span>
                      </a>
                    `,
                  )}
                </div>`
              : nothing}
          </section>
        `,
      )}
    `
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'xkeeper-console-tree': XkeeperConsoleTree
  }
}
