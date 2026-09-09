//! WebSocket wire format for the web console (`webui-api` capability).
//!
//! REST projection types live in [`crate::server`] and are shared verbatim
//! by the console endpoints — one source of truth for both encodings.
//!
//! WS binary frame layout: one type byte, then the payload.
//!   - bit 7 of the type byte marks the payload as zlib-compressed;
//!   - bits 0..6 are the message type (see [`msg_type`] constants).
//! Structured payloads (snapshot, status deltas) are MessagePack; log
//! chunks carry a small fixed header + raw UTF-8 text (no string escaping).

/// WS message types (bits 0..6 of the first frame byte).
pub mod msg_type {
    pub const SNAPSHOT: u8 = 1;
    pub const STATUS: u8 = 2;
    pub const LOG: u8 = 3;
    pub const HEARTBEAT: u8 = 4;
    pub const ERROR: u8 = 5;
    /// A subscriber fell behind: N lines of one stream were dropped for it.
    /// Its own frame type so structural markers never mix with log content.
    pub const LOG_GAP: u8 = 6;
    /// Payloads at least this large are zlib-compressed (flag bit 7).
    pub const COMPRESS_THRESHOLD: usize = 512;
}

/// Encode a WS binary frame: type byte (bit 7 = zlib) + payload.
pub fn encode_ws_message(msg_type: u8, payload: &[u8]) -> Vec<u8> {
    if payload.len() < msg_type::COMPRESS_THRESHOLD {
        let mut frame = Vec::with_capacity(payload.len() + 1);
        frame.push(msg_type);
        frame.extend_from_slice(payload);
        return frame;
    }
    let compressed = {
        use std::io::Write;
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        let _ = enc.write_all(payload);
        enc.finish().unwrap_or_default()
    };
    let mut frame = Vec::with_capacity(compressed.len() + 1);
    frame.push(msg_type | 0x80);
    frame.extend_from_slice(&compressed);
    frame
}

/// Decode the frame header written by [`encode_ws_message`]; returns
/// `(type, decompressed_payload)`. Used by tests and WS clients.
#[cfg_attr(not(test), allow(dead_code))]
pub fn decode_ws_message(frame: &[u8]) -> Result<(u8, Vec<u8>), String> {
    let (first, rest) = frame.split_first().ok_or("empty frame")?;
    let compressed = first & 0x80 != 0;
    let msg_type = first & 0x7f;
    if !compressed {
        return Ok((msg_type, rest.to_vec()));
    }
    let mut dec = flate2::read::ZlibDecoder::new(rest);
    use std::io::Read;
    let mut payload = Vec::new();
    dec.read_to_end(&mut payload)
        .map_err(|e| format!("zlib decompress failed: {e}"))?;
    Ok((msg_type, payload))
}

/// Encode a log-chunk frame: type byte, u16 name length, program name,
/// stream byte (0 = stdout, 1 = stderr), raw UTF-8 chunk.
pub fn encode_log_frame(program: &str, stream: u8, data: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(data.len() + program.len() + 5);
    frame.push(msg_type::LOG);
    frame.extend_from_slice(&(program.len() as u16).to_be_bytes());
    frame.extend_from_slice(program.as_bytes());
    frame.push(stream);
    frame.extend_from_slice(data);
    frame
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn decode_log_frame(frame: &[u8]) -> Result<(String, u8, String), String> {
    if frame.first() != Some(&msg_type::LOG) {
        return Err("not a log frame".into());
    }
    if frame.len() < 4 {
        return Err("truncated log frame".into());
    }
    let name_len = u16::from_be_bytes([frame[1], frame[2]]) as usize;
    let end = 3 + name_len;
    if frame.len() < end + 1 {
        return Err("truncated log frame".into());
    }
    let name = String::from_utf8(frame[3..end].to_vec()).map_err(|e| e.to_string())?;
    let stream = frame[end];
    let data = String::from_utf8(frame[end + 1..].to_vec()).map_err(|e| e.to_string())?;
    Ok((name, stream, data))
}

/// Encode a gap frame: type byte, u16 name length, program name, stream
/// byte (0 = stdout, 1 = stderr), u64 dropped-line count (big-endian).
/// Fixed-width binary header, deliberately not text — a loss marker must
/// never be confusable with program output.
pub fn encode_gap_frame(program: &str, stream: u8, dropped: u64) -> Vec<u8> {
    let mut frame = Vec::with_capacity(program.len() + 12);
    frame.push(msg_type::LOG_GAP);
    frame.extend_from_slice(&(program.len() as u16).to_be_bytes());
    frame.extend_from_slice(program.as_bytes());
    frame.push(stream);
    frame.extend_from_slice(&dropped.to_be_bytes());
    frame
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn decode_gap_frame(frame: &[u8]) -> Result<(String, u8, u64), String> {
    if frame.first() != Some(&msg_type::LOG_GAP) {
        return Err("not a gap frame".into());
    }
    if frame.len() < 4 {
        return Err("truncated gap frame".into());
    }
    let name_len = u16::from_be_bytes([frame[1], frame[2]]) as usize;
    let end = 3 + name_len;
    if frame.len() < end + 1 + 8 {
        return Err("truncated gap frame".into());
    }
    let name = String::from_utf8(frame[3..end].to_vec()).map_err(|e| e.to_string())?;
    let stream = frame[end];
    let mut n = [0u8; 8];
    n.copy_from_slice(&frame[end + 1..end + 9]);
    Ok((name, stream, u64::from_be_bytes(n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_frame_roundtrip_without_compression() {
        let payload = b"tiny".to_vec();
        let frame = encode_ws_message(msg_type::STATUS, &payload);
        assert_eq!(frame[0], msg_type::STATUS);
        let (t, p) = decode_ws_message(&frame).unwrap();
        assert_eq!(t, msg_type::STATUS);
        assert_eq!(p, payload);
    }

    #[test]
    fn ws_frame_roundtrip_with_compression() {
        let payload = vec![b'a'; msg_type::COMPRESS_THRESHOLD * 4];
        let frame = encode_ws_message(msg_type::SNAPSHOT, &payload);
        assert_ne!(frame[0] & 0x80, 0, "large payload must be flagged");
        assert!(
            frame.len() < payload.len(),
            "zlib must shrink repeated data"
        );
        let (t, p) = decode_ws_message(&frame).unwrap();
        assert_eq!(t, msg_type::SNAPSHOT);
        assert_eq!(p, payload);
    }

    #[test]
    fn log_frame_roundtrip() {
        let frame = encode_log_frame("demo-ping", 1, "hello\nworld\n".as_bytes());
        let (name, stream, data) = decode_log_frame(&frame).unwrap();
        assert_eq!(name, "demo-ping");
        assert_eq!(stream, 1);
        assert_eq!(data, "hello\nworld\n");
        // No escaping: raw byte count equals text length plus the header
        // (1 type + 2 name-len + name + 1 stream).
        assert_eq!(
            frame.len(),
            "hello\nworld\n".len() + 1 + 2 + "demo-ping".len() + 1
        );
    }

    #[test]
    fn gap_frame_roundtrip() {
        let frame = encode_gap_frame("demo", 0, 4242);
        let (name, stream, dropped) = decode_gap_frame(&frame).unwrap();
        assert_eq!(name, "demo");
        assert_eq!(stream, 0);
        assert_eq!(dropped, 4242);
        // Fixed header: 1 type + 2 name-len + name + 1 stream + 8 count.
        assert_eq!(frame.len(), 12 + "demo".len());
        assert_ne!(
            decode_log_frame(&frame),
            Ok(("demo".into(), 0, String::new()))
        );
        assert!(decode_gap_frame(&frame[..frame.len() - 1]).is_err());
        assert!(decode_gap_frame(&encode_log_frame("x", 0, b"y")).is_err());
    }
}
