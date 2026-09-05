/**
 * App shell: brand sidebar + routed outlet — the minimal frame that
 * survives the sebas-webui template cleanup. All session-workbench views
 * (project rail, sessions, transcript, composer, settings modal) were
 * deleted with the domain they served; the console surface itself is
 * specified by the `webui-ui` capability and lands in follow-up changes.
 *
 * Link interception is document-level (composedPath) so anchors rendered
 * inside any view's shadow root navigate SPA-side too — shadow retargeting
 * hides them from a shell-scoped listener.
 */

import { LitElement, css, html } from 'lit'
import { customElement, state } from 'lit/decorators.js'
import { matchRoute, navigate, type RouteDef } from './router.js'

// Exported for tests: the route resolution audit iterates these. The
// placeholder console ships a single route; deep links to unknown paths
// fall through to the dashboard like any unmatched path.
export const ROUTES: RouteDef[] = [{ id: 'dashboard', pattern: '/' }]

@customElement('xkeeper-app')
export class XkeeperApp extends LitElement {
  @state() private routeId: string = 'dashboard'

  private params: Record<string, string> = {}
  private onNavigateBound: () => void = () => {}
  private onClick: (e: MouseEvent) => void = () => {}

  static styles = css`
    :host {
      /* 应用框架：100vh 固定高度 + overflow hidden，侧栏与出口区各自
         内部滚动，页面本身不滚。环境渐变背景沿用模板骨架。 */
      display: flex;
      width: 100vw;
      height: 100vh;
      min-height: 0;
      overflow: hidden;
      background: var(--xkeeper-bg);
      background-image: radial-gradient(1100px 480px at 82% -12%, rgba(91, 100, 242, 0.09), transparent 62%),
        radial-gradient(900px 420px at -8% 108%, rgba(56, 209, 221, 0.05), transparent 60%);
      background-attachment: fixed;
      color: var(--xkeeper-text);
    }
    nav {
      width: 220px;
      flex: 0 0 auto;
      position: sticky;
      top: 0;
      height: 100vh;
      box-sizing: border-box; /* 高度吃进 padding，否则 100vh+padding 撑破框架 */
      min-height: 0; /* flex 项默认 min-height:auto 会撑破 100vh 框架 */
      overflow-y: auto;
      background: var(--xkeeper-surface);
      border-right: 1px solid var(--xkeeper-border);
      padding: var(--xkeeper-space-4) var(--xkeeper-space-3);
      display: flex;
      flex-direction: column;
    }
    .brand {
      display: flex;
      align-items: center;
      gap: var(--xkeeper-space-3);
      padding: var(--xkeeper-space-2);
      text-decoration: none;
      color: var(--xkeeper-text-bright);
    }
    .brand .mark {
      display: grid;
      place-items: center;
      width: 28px;
      height: 28px;
      flex: 0 0 auto;
      border-radius: var(--xkeeper-radius-md);
      background: linear-gradient(135deg, var(--xkeeper-accent-strong), #4338ca);
      color: var(--xkeeper-accent-ink);
      font-family: var(--xkeeper-font-mono);
      font-size: 0.9rem;
      font-weight: 700;
      box-shadow:
        var(--xkeeper-shadow-1),
        inset 0 1px 0 rgba(255, 255, 255, 0.18);
    }
    .brand .name {
      font-weight: 700;
      font-size: 1rem;
      letter-spacing: 0.01em;
    }
    .brand .name small {
      display: block;
      font-weight: 500;
      font-size: 0.66rem;
      letter-spacing: 0.09em;
      text-transform: uppercase;
      color: var(--xkeeper-text-faint);
    }
    main {
      flex: 1;
      min-width: 0;
      min-height: 0;
      display: flex;
      flex-direction: column;
    }
    .outlet {
      flex: 1;
      min-height: 0;
      min-width: 0;
      display: flex;
      flex-direction: column;
      position: relative;
    }
    /* Route change mounts a fresh view — replay a soft rise-in. */
    .outlet > * {
      animation: xkeeper-view-in 0.28s var(--xkeeper-ease) both;
      min-height: 0;
    }
    @keyframes xkeeper-view-in {
      from {
        opacity: 0;
        transform: translateY(6px);
      }
      to {
        opacity: 1;
        transform: none;
      }
    }
    @media (prefers-reduced-motion: reduce) {
      .outlet > * {
        animation: none;
      }
    }
    @media (max-width: 640px) {
      :host {
        flex-direction: column;
      }
      nav {
        position: static;
        height: auto;
        width: auto;
        border-right: none;
        border-bottom: 1px solid var(--xkeeper-border);
        padding: var(--xkeeper-space-3) var(--xkeeper-space-4);
      }
      .brand .name small {
        display: none;
      }
    }
  `

  connectedCallback(): void {
    super.connectedCallback()
    this.onNavigateBound = this.onNavigate.bind(this)
    this.onClick = (e: MouseEvent) => {
      if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey)
        return
      for (const node of e.composedPath()) {
        if (!(node instanceof HTMLAnchorElement)) continue
        const href = node.getAttribute('href')
        if (href && href.startsWith('/')) {
          e.preventDefault()
          navigate(href)
        }
        break
      }
    }
    window.addEventListener('popstate', this.onNavigateBound)
    document.addEventListener('click', this.onClick)
    this.onNavigate()
  }

  disconnectedCallback(): void {
    window.removeEventListener('popstate', this.onNavigateBound)
    document.removeEventListener('click', this.onClick)
    super.disconnectedCallback()
  }

  private onNavigate(): void {
    const match = matchRoute(ROUTES, location.pathname)
    if (!match) {
      // Unknown path: render the dashboard rather than a dead screen.
      this.routeId = 'dashboard'
      this.params = {}
    } else {
      this.routeId = match.id
      this.params = match.params
    }
  }

  protected render(): unknown {
    return html`
      <nav aria-label="Primary">
        <a class="brand" href="/">
          <span class="mark" aria-hidden="true">xk</span>
          <span class="name">xkeeper<small>console</small></span>
        </a>
      </nav>
      <main>
        <div class="outlet" data-route=${this.routeId}>
          <xkeeper-dashboard .params=${this.params}></xkeeper-dashboard>
        </div>
      </main>
    `
  }
}
