import { describe, expect, it } from 'vitest'
import {
  decodeFrame,
  decodeGapFrame,
  decodeLogFrame,
  encodeAction,
  textToEntries,
} from './frames.js'
import { MSG, type StatusDoc } from './types.js'
import { encode } from '@msgpack/msgpack'

/**
 * Reference encoder mirroring the Rust side (src/api.rs): type byte with
 * bit-7 zlib flag + payload. Used to build frames the way the daemon does.
 */
async function buildFrame(type: number, payload: Uint8Array): Promise<Uint8Array> {
  if (payload.length < 512) {
    const out = new Uint8Array(payload.length + 1)
    out[0] = type
    out.set(payload, 1)
    return out
  }
  const cs = new CompressionStream('deflate')
  const stream = new Blob([payload as unknown as BlobPart]).stream().pipeThrough(cs)
  const compressed = new Uint8Array(await new Response(stream).arrayBuffer())
  const out = new Uint8Array(compressed.length + 1)
  out[0] = type | 0x80
  out.set(compressed, 1)
  return out
}

function logFrame(program: string, stream: 0 | 1, text: string): Uint8Array {
  const name = new TextEncoder().encode(program)
  const body = new TextEncoder().encode(text)
  const out = new Uint8Array(1 + 2 + name.length + 1 + body.length)
  let i = 0
  out[i++] = MSG.LOG
  out[i++] = name.length >> 8
  out[i++] = name.length & 0xff
  out.set(name, i)
  i += name.length
  out[i++] = stream
  out.set(body, i)
  return out
}

function gapFrame(program: string, stream: 0 | 1, skipped: number): Uint8Array {
  const name = new TextEncoder().encode(program)
  const out = new Uint8Array(1 + 2 + name.length + 1 + 8)
  let i = 0
  out[i++] = MSG.LOG_GAP
  out[i++] = name.length >> 8
  out[i++] = name.length & 0xff
  out.set(name, i)
  i += name.length
  out[i++] = stream
  const view = new DataView(out.buffer)
  view.setBigUint64(i, BigInt(skipped))
  return out
}

describe('frames.decodeFrame', () => {
  it('passes small payloads through with the type byte stripped', async () => {
    const frame = new Uint8Array([MSG.HEARTBEAT, 1, 2, 3])
    const { type, payload } = await decodeFrame(frame)
    expect(type).toBe(MSG.HEARTBEAT)
    expect([...payload]).toEqual([1, 2, 3])
  })

  it('inflates zlib-compressed payloads (bit 7 flag)', async () => {
    const doc = {
      daemon: {
        version: 't', port: 1, apps: 0,
        system: { cpu_percent: null, mem_used_bytes: 0, mem_total_bytes: 0 },
        monitor_interval: 1, uptime_secs: 0, config_source: '/t',
      },
      programs: [],
    }
    const big = encode(doc)
    const padded = new Uint8Array(600)
    padded.set(big.subarray(0, Math.min(big.length, 600)))
    const frame = await buildFrame(MSG.SNAPSHOT, padded)
    expect(frame[0]! & 0x80).not.toBe(0)
    const { type, payload } = await decodeFrame(frame)
    expect(type).toBe(MSG.SNAPSHOT)
    expect([...payload]).toEqual([...padded])
  })

  it('round-trips a large msgpack snapshot through the daemon frame format', async () => {
    const doc = {
      daemon: {
        version: '0.1.0',
        port: 9877,
        apps: 2,
        system: { cpu_percent: 12.5, mem_used_bytes: 1, mem_total_bytes: 2 },
        monitor_interval: 1,
        uptime_secs: 3,
        config_source: '/x.toml',
      },
      programs: [],
    }
    const frame = await buildFrame(MSG.SNAPSHOT, encode(doc))
    const { type, payload } = await decodeFrame(frame)
    expect(type).toBe(MSG.SNAPSHOT)
    // Decode via the same helper the ws-client uses (dynamic import keeps
    // this test honest about the compressed path).
    const { decodeDoc } = await import('./frames.js')
    expect(decodeDoc<StatusDoc>(payload)).toEqual(doc)
  })
})

describe('frames.decodeLogFrame / decodeGapFrame', () => {
  it('splits a multi-line log frame into program/stream/text', () => {
    const frame = logFrame('demo', 1, 'a\nb\nc\n')
    const { program, stream, text } = decodeLogFrame(frame)
    expect(program).toBe('demo')
    expect(stream).toBe(1)
    expect(text).toBe('a\nb\nc\n')
  })

  it('rejects frames with a wrong type byte or truncated header', () => {
    const frame = logFrame('demo', 0, 'x')
    expect(() => decodeLogFrame(frame.subarray(0, 3))).toThrow()
    const wrong = frame.slice()
    wrong[0] = MSG.LOG_GAP
    expect(() => decodeLogFrame(wrong)).toThrow()
  })

  it('decodes a gap frame with a >32-bit skip count', () => {
    const frame = gapFrame('fire', 0, 5_000_000_000)
    const { program, stream, skipped } = decodeGapFrame(frame)
    expect(program).toBe('fire')
    expect(stream).toBe(0)
    expect(skipped).toBe(5_000_000_000)
    expect(() => decodeGapFrame(frame.subarray(0, 8))).toThrow()
  })
})

describe('frames.encodeAction', () => {
  it('emits JSON the server parses (serde ClientAction)', () => {
    const bytes = encodeAction('subscribe', 'p', 1)
    const parsed = JSON.parse(new TextDecoder().decode(bytes))
    expect(parsed).toEqual({ action: 'subscribe', program: 'p', stream: 'err' })
  })
})

describe('frames.textToEntries', () => {
  it('splits complete lines and keeps the trailing partial as carry', () => {
    const carry: string[] = []
    const first = textToEntries('one\ntwo\nthr', carry)
    expect(first).toEqual([{ kind: 'line', text: 'one' }, { kind: 'line', text: 'two' }])
    expect(carry).toEqual(['thr'])
    const second = textToEntries('ee\n', carry)
    expect(second).toEqual([{ kind: 'line', text: 'three' }])
    expect(carry).toEqual([])
  })
})
