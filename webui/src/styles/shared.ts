/**
 * Shared view style kit. Every routed view composes `viewStyles` with its
 * own `css` fragment so panels, tables, empty states, skeletons, callouts
 * and key/value rows look identical everywhere. These live inside each
 * component's shadow root, so the focus-visible rules here cover anchors
 * and focusable elements that the document-level rule cannot reach.
 */

import { css } from 'lit'

export const viewStyles = css`
  :host {
    display: block;
  }

  /* ---- Page header ------------------------------------------------- */
  .page-head {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: var(--xkeeper-space-4);
    flex-wrap: wrap;
    margin-bottom: var(--xkeeper-space-5);
  }
  .page-title {
    margin: 0;
    font-size: 1.25rem;
    font-weight: 650;
    letter-spacing: -0.015em;
    color: var(--xkeeper-text-bright);
  }
  .page-sub {
    margin: var(--xkeeper-space-1) 0 0;
    color: var(--xkeeper-text-dim);
    font-size: 0.875rem;
  }

  /* ---- Panels ------------------------------------------------------- */
  .panel {
    background: var(--xkeeper-surface);
    border: 1px solid var(--xkeeper-border);
    border-radius: var(--xkeeper-radius-lg);
    box-shadow: var(--xkeeper-shadow-1);
    overflow: hidden;
  }
  /* 8.1: at narrow widths a wide child (e.g. a data table) must stay
     reachable — clip only vertically, let the user swipe horizontally. */
  @media (max-width: 640px) {
    .panel {
      overflow-x: auto;
    }
  }
  .panel-pad {
    padding: var(--xkeeper-space-4);
  }
  .panel-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--xkeeper-space-3);
    flex-wrap: wrap;
    padding: var(--xkeeper-space-3) var(--xkeeper-space-4);
    border-bottom: 1px solid var(--xkeeper-border);
  }
  .panel-title {
    margin: 0;
    font-size: 0.95rem;
    font-weight: 600;
    color: var(--xkeeper-text-bright);
  }
  .panel-caption {
    color: var(--xkeeper-text-faint);
    font-size: 0.8rem;
  }

  /* ---- Tables ------------------------------------------------------- */
  table {
    width: 100%;
    border-collapse: collapse;
  }
  th,
  td {
    text-align: left;
    padding: var(--xkeeper-space-3) var(--xkeeper-space-4);
    border-bottom: 1px solid var(--xkeeper-border);
    vertical-align: middle;
  }
  th {
    color: var(--xkeeper-text-faint);
    font-weight: 550;
    font-size: 0.7rem;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    background: var(--xkeeper-surface-2);
    white-space: nowrap;
  }
  tbody tr {
    transition: background var(--xkeeper-dur) var(--xkeeper-ease);
  }
  tbody tr:hover {
    background: var(--xkeeper-surface-2);
  }
  tbody tr:last-child td {
    border-bottom: none;
  }
  /* Status accent on the leading edge of a row (set data-status on the tr). */
  tbody tr[data-status] td:first-child {
    box-shadow: inset 2px 0 0 0 var(--xkeeper-status-dormant);
  }
  tbody tr[data-status='starting'] td:first-child {
    box-shadow: inset 2px 0 0 0 var(--xkeeper-status-starting);
  }
  tbody tr[data-status='queued'] td:first-child {
    box-shadow: inset 2px 0 0 0 var(--xkeeper-status-queued);
  }
  tbody tr[data-status='working'] td:first-child {
    box-shadow: inset 2px 0 0 0 var(--xkeeper-status-working);
  }
  tbody tr[data-status='done'] td:first-child {
    box-shadow: inset 2px 0 0 0 var(--xkeeper-status-done);
  }
  tbody tr[data-status='failed'] td:first-child {
    box-shadow: inset 2px 0 0 0 var(--xkeeper-status-failed);
  }
  tbody tr[data-status='dormant'] td:first-child {
    box-shadow: inset 2px 0 0 0 var(--xkeeper-status-dormant);
  }

  /* ---- Typography helpers ------------------------------------------ */
  .mono {
    font-family: var(--xkeeper-font-mono);
    font-size: 0.82rem;
  }
  .tnum {
    font-variant-numeric: tabular-nums;
  }
  .dim {
    color: var(--xkeeper-text-dim);
  }

  /* ---- Links -------------------------------------------------------- */
  a {
    color: var(--xkeeper-accent);
    text-decoration: none;
    transition: color var(--xkeeper-dur) var(--xkeeper-ease);
  }
  a:hover {
    color: var(--xkeeper-accent-hover);
    text-decoration: underline;
    text-underline-offset: 3px;
  }
  a:focus-visible {
    outline: var(--xkeeper-focus-ring);
    outline-offset: 2px;
    border-radius: var(--xkeeper-radius-sm);
  }
  button:focus-visible,
  textarea:focus-visible,
  input:focus-visible,
  select:focus-visible,
  [tabindex]:focus-visible {
    outline: var(--xkeeper-focus-ring);
    outline-offset: 2px;
  }

  /* ---- Empty states -------------------------------------------------- */
  .empty {
    padding: var(--xkeeper-space-10) var(--xkeeper-space-6);
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--xkeeper-space-2);
    text-align: center;
    color: var(--xkeeper-text-dim);
  }
  .empty .glyph {
    display: grid;
    place-items: center;
    width: 44px;
    height: 44px;
    border-radius: var(--xkeeper-radius-full);
    background: var(--xkeeper-surface-2);
    border: 1px solid var(--xkeeper-border);
    color: var(--xkeeper-text-faint);
    margin-bottom: var(--xkeeper-space-2);
  }
  .empty .title {
    color: var(--xkeeper-text-bright);
    font-weight: 600;
    font-size: 0.95rem;
  }
  .empty .hint {
    margin: 0;
    font-size: 0.85rem;
    max-width: 42ch;
  }
  .empty .cta {
    margin-top: var(--xkeeper-space-3);
  }

  /* ---- Loading skeletons --------------------------------------------- */
  .skel {
    border-radius: var(--xkeeper-radius-sm);
    background: linear-gradient(
      90deg,
      var(--xkeeper-surface-2) 25%,
      var(--xkeeper-surface-3) 45%,
      var(--xkeeper-surface-2) 65%
    );
    background-size: 200% 100%;
    animation: xkeeper-shimmer 1.4s ease-in-out infinite;
  }
  .skel-row {
    display: flex;
    gap: var(--xkeeper-space-4);
    padding: var(--xkeeper-space-4);
    border-bottom: 1px solid var(--xkeeper-border);
  }
  .skel-row:last-child {
    border-bottom: none;
  }
  .skel-line {
    height: 12px;
  }
  @keyframes xkeeper-shimmer {
    from {
      background-position: 180% 0;
    }
    to {
      background-position: -80% 0;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .skel {
      animation: none;
    }
  }

  /* ---- Callouts ------------------------------------------------------ */
  .callout {
    display: flex;
    align-items: flex-start;
    gap: var(--xkeeper-space-2);
    padding: var(--xkeeper-space-3) var(--xkeeper-space-4);
    border-radius: var(--xkeeper-radius-md);
    border: 1px solid;
    font-size: 0.875rem;
    margin: 0 0 var(--xkeeper-space-4);
  }
  .callout svg {
    flex: 0 0 auto;
    margin-top: 2px;
  }
  .callout-error {
    color: var(--xkeeper-status-failed);
    background: var(--xkeeper-status-failed-bg);
    border-color: var(--xkeeper-status-failed-border);
  }
  .callout-warn,
  .callout-warning {
    color: var(--xkeeper-status-working);
    background: var(--xkeeper-status-working-bg);
    border-color: var(--xkeeper-status-working-border);
  }
  .callout-info {
    color: var(--xkeeper-status-done);
    background: var(--xkeeper-status-done-bg);
    border-color: var(--xkeeper-status-done-border);
  }

  /* ---- Chips ---------------------------------------------------------- */
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 3px 10px;
    border-radius: var(--xkeeper-radius-full);
    border: 1px solid var(--xkeeper-border);
    background: var(--xkeeper-surface-2);
    color: var(--xkeeper-text-dim);
    font-size: 0.78rem;
    white-space: nowrap;
  }
  .chip .dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--xkeeper-text-faint);
  }
  .chip b {
    color: var(--xkeeper-text-bright);
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }

  /* ---- Key/value rows -------------------------------------------------- */
  .kv {
    display: grid;
    grid-template-columns: minmax(140px, 220px) 1fr;
    gap: var(--xkeeper-space-2) var(--xkeeper-space-6);
    padding: var(--xkeeper-space-3) 0;
    border-bottom: 1px solid var(--xkeeper-border);
    margin: 0;
  }
  .kv:last-child {
    border-bottom: none;
  }
  .kv dt {
    color: var(--xkeeper-text-dim);
    font-size: 0.875rem;
  }
  .kv dd {
    margin: 0;
    color: var(--xkeeper-text);
    font-variant-numeric: tabular-nums;
    overflow-wrap: anywhere;
  }
`
