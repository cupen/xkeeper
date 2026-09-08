/**
 * Operator-facing status badge. Renders the backend-owned status projection
 * (label + slug + glyph) as a tinted pill: glyph shape, colored dot, and the
 * word — so state survives greyscale and colour-blindness (shape + word +
 * colour). Pill background/border/text come from the per-slug status tokens
 * via the data-status attribute; the dot keeps an inline `--xkeeper-status-*`
 * reference (stylesheet hook, mirrors the SSR projection).
 *
 * The mapping lives server-side (single source); this component only
 * renders what the API sends.
 */

import { LitElement, css, html, nothing } from 'lit'
import { customElement, property } from 'lit/decorators.js'

/**
 * The status vocabulary is owned by the backend (single source); the badge
 * renders whatever slug the API sends. Known slugs get per-slug tokens from
 * the stylesheet; unknown slugs fall back to the neutral default.
 */
export type StatusSlug = string

@customElement('xkeeper-status-badge')
export class XkeeperStatusBadge extends LitElement {
  @property() slug: StatusSlug = ''
  @property() label = ''
  @property() glyph = ''

  static styles = css`
    .badge {
      display: inline-flex;
      align-items: center;
      gap: 7px;
      padding: 3px 10px 3px 9px;
      border-radius: var(--xkeeper-radius-full, 999px);
      border: 1px solid var(--xkeeper-status-queued-border, rgba(56, 209, 221, 0.3));
      background: var(--xkeeper-status-queued-bg, rgba(56, 209, 221, 0.1));
      color: var(--xkeeper-status-queued, #38d1dd);
      font-size: 0.78rem;
      font-weight: 550;
      letter-spacing: 0.01em;
      line-height: 1.4;
      font-variant-numeric: tabular-nums;
      white-space: nowrap;
      transition:
        background var(--xkeeper-dur, 150ms) var(--xkeeper-ease, ease),
        border-color var(--xkeeper-dur, 150ms) var(--xkeeper-ease, ease);
    }
    .badge[data-status='starting'] {
      background: var(--xkeeper-status-starting-bg);
      border-color: var(--xkeeper-status-starting-border);
      color: var(--xkeeper-status-starting);
    }
    .badge[data-status='queued'] {
      background: var(--xkeeper-status-queued-bg);
      border-color: var(--xkeeper-status-queued-border);
      color: var(--xkeeper-status-queued);
    }
    .badge[data-status='working'] {
      background: var(--xkeeper-status-working-bg);
      border-color: var(--xkeeper-status-working-border);
      color: var(--xkeeper-status-working);
    }
    .badge[data-status='done'] {
      background: var(--xkeeper-status-done-bg);
      border-color: var(--xkeeper-status-done-border);
      color: var(--xkeeper-status-done);
    }
    .badge[data-status='failed'] {
      background: var(--xkeeper-status-failed-bg);
      border-color: var(--xkeeper-status-failed-border);
      color: var(--xkeeper-status-failed);
    }
    .badge[data-status='dormant'] {
      background: var(--xkeeper-status-dormant-bg);
      border-color: var(--xkeeper-status-dormant-border);
      color: var(--xkeeper-status-dormant);
    }
    .glyph {
      font-size: 0.9em;
      line-height: 1;
    }
    .dot {
      position: relative;
      width: 7px;
      height: 7px;
      border-radius: 50%;
      background: var(--xkeeper-status-queued, #38d1dd);
      flex: 0 0 auto;
    }
    /* Live pulse on the working status — CSS only, honours reduced motion. */
    .badge[data-status='working'] .dot::before {
      content: '';
      position: absolute;
      inset: -3px;
      border-radius: 50%;
      border: 1px solid var(--xkeeper-status-working, currentColor);
      animation: xkeeper-ping 1.6s var(--xkeeper-ease, ease) infinite;
    }
    @keyframes xkeeper-ping {
      0% {
        transform: scale(0.55);
        opacity: 0.9;
      }
      80%,
      100% {
        transform: scale(1.7);
        opacity: 0;
      }
    }
    @media (prefers-reduced-motion: reduce) {
      .badge[data-status='working'] .dot::before {
        animation: none;
        opacity: 0;
      }
    }
    .label {
      font-size: 0.92em;
    }
  `

  private get statusColor(): string {
    const slug = this.slug.replace(/[^a-z]/g, '')
    return `var(--xkeeper-status-${slug}, var(--xkeeper-status-queued, #b8860b))`
  }

  render() {
    // Glyph and label are always rendered: colour is never the only channel.
    return html`<span class="badge" data-status=${this.slug}>
      <span class="dot" style=${`background: ${this.statusColor}`} aria-hidden="true"></span>
      <span class="glyph" aria-hidden="true">${this.glyph || nothing}</span>
      <span class="label">${this.label || this.slug}</span>
    </span>`
  }
}

declare global {
  interface HTMLElementTagNameMap {
    'xkeeper-status-badge': XkeeperStatusBadge
  }
}
