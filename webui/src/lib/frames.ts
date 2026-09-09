/**
 * Wire-frame codec for the `/ws` binary channel — the browser-side twin of
 * src/api.rs. Frame layout: one type byte (bit 7 = zlib-compressed payload,
 * bits 0..6 = message type), then the payload. Structured payloads
 * (snapshot, status deltas) are MessagePack; log/gap frames carry a small
 * fixed header + raw UTF-8.
 *
 * Decoding is async (zlib inflate via DecompressionStream), so callers get
 * Promises. Order preservation matters: the WsClient chains frame
 * processing so a compressed frame never overtakes an earlier one.
 */

import { decode as msgpackDecode } from '@msgpack/msgpack'
import { MSG, type LogEntry, type StatusDoc } from './types.js'

async function inflate(data: Uint8Array): Promise<Uint8Array> {
  const ds = new DecompressionStream('deflate')
  // Copy into a fresh buffer: the underlying ArrayBuffer may be a view into
  // a larger pool that streams dislike.
  const bytes = new Uint8Array(data.length)
  bytes.set(data)
  const stream = new Blob([bytes]).stream().pipeThrough(ds)
  return new Uint8Array(await new Response(stream).arrayBuffer())
}

export interface DecodedFrame {
  type: number
  payload: Uint8Array
}

/** Decode one WS binary frame (type byte header + payload). */
export async function decodeFrame(data: Uint8Array): Promise<DecodedFrame> {
  if (data.length === 0) throw new Error('empty ws frame')
  const first = data[0]!
  const compressed = (first & 0x80) !== 0
  const type = first & 0x7f
  const rest = data.subarray(1)
  const payload = compressed ? await inflate(rest) : rest
  return { type, payload }
}

/** Decode a snapshot/status payload (MessagePack) into the shared types. */
export function decodeDoc<T>(payload: Uint8Array): T {
  return msgpackDecode(payload) as T
}

/** Full log/gap frame reconstruction for decode helpers. */
function readName(frame: Uint8Array, offset: number): { name: string; end: number } {
  if (frame.length < offset + 2) throw new Error('truncated log frame')
  const nameLen = (frame[offset]! << 8) | frame[offset + 1]!
  const start = offset + 2
  const end = start + nameLen
  if (frame.length < end) throw new Error('truncated log frame')
  const name = new TextDecoder().decode(frame.subarray(start, end))
  return { name, end }
}

/**
 * Split a LOG frame (type byte already included) into (program, stream,
 * text). The text is raw UTF-8 with embedded newlines — one frame carries
 * many lines.
 */
export function decodeLogFrame(frame: Uint8Array): { program: string; stream: 0 | 1; text: string } {
  if (frame.length < 4 || frame[0] !== MSG.LOG) throw new Error('not a log frame')
  const { name, end } = readName(frame, 1)
  if (frame.length < end + 1) throw new Error('truncated log frame')
  const stream = frame[end] as 0 | 1
  const text = new TextDecoder().decode(frame.subarray(end + 1))
  return { program: name, stream, text }
}

/** Decode a LOG_GAP frame: program, stream, and how many lines were lost. */
export function decodeGapFrame(frame: Uint8Array): { program: string; stream: 0 | 1; skipped: number } {
  if (frame.length < 12 || frame[0] !== MSG.LOG_GAP) throw new Error('not a gap frame')
  const { name, end } = readName(frame, 1)
  if (frame.length < end + 9) throw new Error('truncated gap frame')
  const stream = frame[end] as 0 | 1
  let skipped = 0
  for (let i = end + 1; i < end + 9; i++) {
    skipped = skipped * 256 + frame[i]!
  }
  return { program: name, stream, skipped }
}

/**
 * Build a subscribe/unsubscribe action (JSON binary — tiny, and human
 * debuggable in the devtools).
 */
export function encodeAction(action: 'subscribe' | 'unsubscribe', program: string, stream: 0 | 1): Uint8Array {
  // The server's ClientAction only accepts "out"/"err" (web.rs stream_of).
  const body = JSON.stringify({ action, program, stream: stream === 0 ? 'out' : 'err' })
  const out = new Uint8Array(body.length + 1)
  out[0] = 0 // server ignores the leading byte for client actions
  out.set(new TextEncoder().encode(body), 1)
  return out.subarray(1)
}

/**
 * Split raw log text (one or more complete lines, '\n'-terminated) into
 * viewer entries. Every '\n'-terminated chunk is a line; a trailing partial
 * line is buffered by the caller via `carry` (mutated in place) so frames
 * that split mid-line stay coherent.
 */
export function textToEntries(text: string, carry: string[]): LogEntry[] {
  const entries: LogEntry[] = []
  let rest = text
  if (carry.length > 0) {
    rest = carry.pop()! + text
  }
  let start = 0
  for (let i = 0; i < rest.length; i++) {
    if (rest.charCodeAt(i) === 10) {
      entries.push({ kind: 'line', text: rest.slice(start, i) })
      start = i + 1
    }
  }
  if (start < rest.length) {
    carry.push(rest.slice(start)) // partial line: wait for the next frame
  }
  return entries
}

/** Type guard helper for tests: decode a full snapshot frame. */
export function isSnapshot(doc: unknown): doc is StatusDoc {
  return typeof doc === 'object' && doc !== null && 'daemon' in doc && 'programs' in doc
}
