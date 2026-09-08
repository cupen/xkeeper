/**
 * Placeholder dashboard. The real console surface (program overview,
 * status badges, controls, logs — the `webui-ui` capability) lands in
 * follow-up changes; this view only proves the frontend→backend chain
 * (`GET /api/health`) and states honestly that the console is under
 * construction. No fake data, no dead controls.
 */

import { LitElement, css, html } from 'lit'
import { customElement, state } from 'lit/decorators.js'

@customElement('xkeeper-dashboard')
export class XkeeperDashboard extends LitElement {
  /** null = probe pending; true/false = backend reachability. */
  @state() private backendUp: boolean | null = null

  static styles = css`
    :host {
      flex: 1;
      min-height: 0;
      overflow-y: auto;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      gap: var(--xkeeper-space-4);
      padding: var(--xkeeper-space-8) var(--xkeeper-space-5);
      box-sizing: border-box;
      text-align: center;
    }
    h1 {
      margin: 0;
      font-size: 1.35rem;
      font-weight: 700;
      color: var(--xkeeper-text-bright);
    }
    p {
      margin: 0;
      max-width: 560px;
      line-height: 1.6;
      color: var(--xkeeper-text-dim);
      font-size: 0.92rem;
    }
    code {
      font-family: var(--xkeeper-font-mono);
      font-size: 0.85em;
      color: var(--xkeeper-text);
      background: var(--xkeeper-well);
      border: 1px solid var(--xkeeper-border);
      border-radius: var(--xkeeper-radius-sm, 6px);
      padding: 1px 6px;
    }
    .health {
      display: inline-flex;
      align-items: center;
      gap: 8px;
      padding: 6px 14px;
      border-radius: var(--xkeeper-radius-full, 999px);
      border: 1px solid var(--xkeeper-border);
      background: var(--xkeeper-surface);
      font-size: 0.85rem;
      color: var(--xkeeper-text-dim);
    }
    .health .dot {
      width: 8px;
      height: 8px;
      border-radius: 50%;
      flex: 0 0 auto;
    }
    .health[data-up='true'] .dot {
      background: #3fd68f;
      box-shadow: 0 0 8px rgba(63, 214, 143, 0.6);
    }
    .health[data-up='false'] .dot {
      background: #ff6b6b;
    }
    .health[data-up='pending'] .dot {
      background: var(--xkeeper-text-faint);
    }
  `

  connectedCallback(): void {
    super.connectedCallback()
    this.probe()
  }

  private async probe(): Promise<void> {
    try {
      const res = await fetch('/api/health')
      this.backendUp = res.ok
    } catch {
      this.backendUp = false
    }
  }

  protected render(): unknown {
    const up = this.backendUp
    return html`
      <h1>Web 控制台建设中</h1>
      <p>
        xkeeper 的控制台界面（程序总览、状态徽章、启停控制、日志查看）按
        <code>openspec</code> 的 <code>webui-ui</code> 能力规格在后续变更中实现。
        本占位页仅用于验证前端与后端 <code>/api/health</code> 的链路。
      </p>
      <div class="health" data-up=${up === null ? 'pending' : String(up)}>
        <span class="dot" aria-hidden="true"></span>
        <span>
          ${up === null ? '正在探测后端…' : up ? '后端可达 · /api/health ok' : '后端不可达 · 请启动 xkeeper webui'}
        </span>
      </div>
    `
  }
}
