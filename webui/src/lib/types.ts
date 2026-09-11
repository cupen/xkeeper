/**
 * Client-side mirror of the daemon's shared status projection (server.rs).
 * Field names/semantics MUST stay identical to the Rust serde structs —
 * JSON (REST) and MessagePack (WS) both decode into these types.
 *
 * Metric fields (cpu_percent / mem_bytes / log_rate / system) are nullable:
 * `null` means "no data yet" (not running / first sample / sampler gone) and
 * must render as a placeholder, never as 0.
 */

export interface StreamRate {
  w1: number
  w10: number
  w60: number
  w300: number
}

export interface LogRates {
  out: StreamRate
  err: StreamRate
}

export interface SystemMetrics {
  cpu_percent: number | null
  mem_used_bytes: number
  mem_total_bytes: number
}

export interface DaemonInfo {
  version: string
  port: number
  apps: number
  system: SystemMetrics
  monitor_interval: number
  uptime_secs: number
  config_source: string
}

export interface ProgramInfo {
  app: string
  name: string
  state: string
  pid: number | null
  unhealthy: boolean
  uptime_secs: number
  total_exits: number
  restart_backoff: number
  last_exit: string | null
  fatal_reason: string | null
  wait_reason: string | null
  cpu_percent: number | null
  mem_bytes: number | null
  log_rate: LogRates
  /** Declared command and args (detail display). */
  command: string
  args: string[]
  /** Working directory (empty = inherited). */
  work_dir: string
}

export interface PendingProgram {
  app: string
  program: string
  running: boolean
}

/** Detected-but-not-applied config changes (server: supervisor::PendingDoc). */
export interface PendingDoc {
  programs: PendingProgram[]
  apps_added: string[]
  apps_removed: string[]
  daemon_hints: string[]
  errors: string[]
}

export interface StatusDoc {
  daemon: DaemonInfo
  programs: ProgramInfo[]
  pending: PendingDoc
}

/** Log stream direction (matches the server's stream byte). */
export type StreamId = 0 | 1

/** WS message type bytes (must match src/api.rs msg_type). */
export const MSG = {
  SNAPSHOT: 1,
  STATUS: 2,
  LOG: 3,
  HEARTBEAT: 4,
  ERROR: 5,
  LOG_GAP: 6,
} as const

/** Entries a log viewer renders: real lines, or an explicit loss marker. */
export type LogEntry = { kind: 'line'; text: string } | { kind: 'gap'; skipped: number }
