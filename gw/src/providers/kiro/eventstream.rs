//! AWS `application/vnd.amazon.eventstream` framing (CodeWhisperer generation responses).

use super::digest::crc32;
use std::collections::BTreeMap;

const PRELUDE_LEN: usize = 8;
const PRELUDE_CRC_LEN: usize = 4;
const MESSAGE_CRC_LEN: usize = 4;
const HEADER_BLOCK_OFFSET: usize = PRELUDE_LEN + PRELUDE_CRC_LEN;
const MIN_MESSAGE_LEN: usize = HEADER_BLOCK_OFFSET + MESSAGE_CRC_LEN;
const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;
const MAX_HEADERS_LEN: usize = 128 * 1024;

/// One decoded eventstream message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Header names keep their wire spelling, e.g. `:event-type`.
    pub headers: BTreeMap<String, String>,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub fn payload_str(&self) -> String {
        String::from_utf8_lossy(&self.payload).into_owned()
    }
}

/// A framing error; the stream cannot be trusted after one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "eventstream: {}", self.0)
    }
}

impl std::error::Error for Error {}

fn fail<T>(message: impl Into<String>) -> Result<T, Error> {
    Err(Error(message.into()))
}

/// Incremental decoder; feed chunks of any size.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Frames completed by `chunk`, in order.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Message>, Error> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut offset = 0;
        while self.buf.len() - offset >= 4 {
            let total = u32::from_be_bytes(
                self.buf[offset..offset + 4]
                    .try_into()
                    .expect("four bytes read"),
            ) as usize;
            if total < MIN_MESSAGE_LEN {
                return fail(format!("total length {total} below minimum"));
            }
            if total > MAX_MESSAGE_LEN {
                return fail(format!("total length {total} exceeds maximum"));
            }
            if self.buf.len() - offset >= HEADER_BLOCK_OFFSET {
                let headers_len = u32::from_be_bytes(
                    self.buf[offset + 4..offset + 8]
                        .try_into()
                        .expect("four bytes read"),
                ) as usize;
                let prelude_crc = u32::from_be_bytes(
                    self.buf[offset + 8..offset + 12]
                        .try_into()
                        .expect("four bytes read"),
                );
                if crc32(&self.buf[offset..offset + PRELUDE_LEN]) != prelude_crc {
                    return fail("prelude CRC mismatch");
                }
                if headers_len > MAX_HEADERS_LEN {
                    return fail(format!("headers length {headers_len} exceeds maximum"));
                }
                if headers_len > total - MIN_MESSAGE_LEN {
                    return fail("headers length exceeds frame payload");
                }
            }
            if self.buf.len() - offset < total {
                break;
            }
            let frame = &self.buf[offset..offset + total];
            out.push(decode_message(frame)?);
            offset += total;
        }
        self.buf.drain(..offset);
        if self.buf.len() > MAX_MESSAGE_LEN {
            return fail("buffered frame exceeds maximum");
        }
        Ok(out)
    }

    /// Reject a stream that ended mid-frame.
    pub fn finish(&mut self) -> Result<(), Error> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            fail("truncated message at end of stream")
        }
    }
}

fn decode_message(frame: &[u8]) -> Result<Message, Error> {
    if frame.len() < MIN_MESSAGE_LEN {
        return fail("frame too short");
    }
    let total = u32::from_be_bytes(frame[0..4].try_into().expect("four bytes read")) as usize;
    if total != frame.len() {
        return fail(format!("framed length {total} != buffer {}", frame.len()));
    }
    let headers_len = u32::from_be_bytes(frame[4..8].try_into().expect("four bytes read")) as usize;
    if headers_len > total - MIN_MESSAGE_LEN {
        return fail("headers length exceeds frame payload");
    }
    let message_crc = u32::from_be_bytes(
        frame[total - MESSAGE_CRC_LEN..]
            .try_into()
            .expect("four bytes read"),
    );
    if crc32(&frame[..total - MESSAGE_CRC_LEN]) != message_crc {
        return fail("message CRC mismatch");
    }
    let headers = parse_headers(&frame[HEADER_BLOCK_OFFSET..HEADER_BLOCK_OFFSET + headers_len])?;
    let payload = frame[HEADER_BLOCK_OFFSET + headers_len..total - MESSAGE_CRC_LEN].to_vec();
    Ok(Message { headers, payload })
}

fn parse_headers(buf: &[u8]) -> Result<BTreeMap<String, String>, Error> {
    let mut out = BTreeMap::new();
    let mut p = 0;
    while p < buf.len() {
        let name_len = usize::from(
            *buf.get(p)
                .ok_or_else(|| Error("truncated header name length".into()))?,
        );
        p += 1;
        let name = read_str(buf, &mut p, name_len, "header name")?;
        let kind = *buf
            .get(p)
            .ok_or_else(|| Error("truncated header type".into()))?;
        p += 1;
        let value = match kind {
            0 => "true".to_owned(),
            1 => "false".to_owned(),
            2 => {
                let byte = take(buf, &mut p, 1, "byte value")?[0];
                (byte as i8).to_string()
            }
            3 => {
                let raw = take(buf, &mut p, 2, "short value")?;
                i16::from_be_bytes([raw[0], raw[1]]).to_string()
            }
            4 => {
                let raw = take(buf, &mut p, 4, "integer value")?;
                i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]).to_string()
            }
            5 => {
                let raw = take(buf, &mut p, 8, "long value")?;
                let mut word = [0u8; 8];
                word.copy_from_slice(raw);
                i64::from_be_bytes(word).to_string()
            }
            6 => {
                let len = length16(buf, &mut p, "byte-array length")?;
                base64(take(buf, &mut p, len, "byte-array value")?)
            }
            7 => {
                let len = length16(buf, &mut p, "string length")?;
                read_str(buf, &mut p, len, "string value")?
            }
            8 => {
                let raw = take(buf, &mut p, 8, "timestamp value")?;
                let mut word = [0u8; 8];
                word.copy_from_slice(raw);
                i64::from_be_bytes(word).to_string()
            }
            9 => {
                let raw = take(buf, &mut p, 16, "uuid value")?;
                let hex: Vec<String> = raw.iter().map(|byte| format!("{byte:02x}")).collect();
                format!(
                    "{}-{}-{}-{}-{}",
                    hex[0..4].concat(),
                    hex[4..6].concat(),
                    hex[6..8].concat(),
                    hex[8..10].concat(),
                    hex[10..16].concat()
                )
            }
            other => return fail(format!("unknown header value type {other}")),
        };
        out.insert(name, value);
    }
    Ok(out)
}

fn take<'a>(buf: &'a [u8], p: &mut usize, n: usize, label: &str) -> Result<&'a [u8], Error> {
    if *p + n > buf.len() {
        return fail(format!("truncated {label}"));
    }
    let slice = &buf[*p..*p + n];
    *p += n;
    Ok(slice)
}

fn length16(buf: &[u8], p: &mut usize, label: &str) -> Result<usize, Error> {
    let raw = take(buf, p, 2, label)?;
    Ok(usize::from(u16::from_be_bytes([raw[0], raw[1]])))
}

fn read_str(buf: &[u8], p: &mut usize, n: usize, label: &str) -> Result<String, Error> {
    let raw = take(buf, p, n, label)?;
    Ok(String::from_utf8_lossy(raw).into_owned())
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
pub(crate) fn encode_message(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut block = Vec::new();
    for (name, value) in headers {
        block.push(name.len() as u8);
        block.extend_from_slice(name.as_bytes());
        block.push(7);
        block.extend_from_slice(&(value.len() as u16).to_be_bytes());
        block.extend_from_slice(value.as_bytes());
    }
    let total = HEADER_BLOCK_OFFSET + block.len() + payload.len() + MESSAGE_CRC_LEN;
    let mut frame = vec![0u8; total];
    frame[0..4].copy_from_slice(&(total as u32).to_be_bytes());
    frame[4..8].copy_from_slice(&(block.len() as u32).to_be_bytes());
    let prelude = crc32(&frame[..PRELUDE_LEN]);
    frame[8..12].copy_from_slice(&prelude.to_be_bytes());
    frame[HEADER_BLOCK_OFFSET..HEADER_BLOCK_OFFSET + block.len()].copy_from_slice(&block);
    let payload_at = HEADER_BLOCK_OFFSET + block.len();
    frame[payload_at..payload_at + payload.len()].copy_from_slice(payload);
    let crc = crc32(&frame[..total - MESSAGE_CRC_LEN]);
    frame[total - MESSAGE_CRC_LEN..].copy_from_slice(&crc.to_be_bytes());
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_messages_split_across_chunks() {
        let first = encode_message(
            &[
                (":message-type", "event"),
                (":event-type", "assistantResponseEvent"),
            ],
            br#"{"content":"hi"}"#,
        );
        let second = encode_message(&[(":message-type", "event")], b"{}");
        let mut stream = first.clone();
        stream.extend_from_slice(&second);

        let mut decoder = Decoder::new();
        let mut messages = Vec::new();
        for chunk in stream.chunks(3) {
            messages.extend(decoder.push(chunk).unwrap());
        }
        decoder.finish().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0].header(":event-type"),
            Some("assistantResponseEvent")
        );
        assert_eq!(messages[0].payload, br#"{"content":"hi"}"#);
        assert_eq!(messages[1].payload, b"{}");
    }

    #[test]
    fn rejects_bad_crc_and_truncation() {
        let frame = encode_message(&[(":message-type", "event")], b"{}");
        let mut corrupt = frame.clone();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xff;
        assert!(Decoder::new().push(&corrupt).is_err());

        let mut decoder = Decoder::new();
        decoder.push(&frame[..frame.len() - 2]).unwrap();
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn keeps_non_string_header_types() {
        let payload = encode_message(&[("bool", "true"), ("str", "value")], b"");
        let mut decoder = Decoder::new();
        let messages = decoder.push(&payload).unwrap();
        assert_eq!(messages[0].header("bool"), Some("true"));
        assert_eq!(messages[0].header("str"), Some("value"));
    }
}
