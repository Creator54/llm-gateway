//! Incremental server-sent events reader.

use super::Error;

/// Longest line tail or event payload kept in memory.
const MAX_BYTES: usize = 8 * 1024 * 1024;

/// Splits bytes into the `data` payloads of complete events.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Incomplete tail of the current line, kept between chunks.
    tail: Vec<u8>,
    /// `data` lines of the event being assembled, joined with newlines.
    data: String,
    has_data: bool,
}

impl SseParser {
    /// Start an empty parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes and return the payloads of the events completed by them.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, Error> {
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(chunk);
        let mut done = Vec::new();
        let mut start = 0;
        while let Some(pos) = buf[start..].iter().position(|&byte| byte == b'\n') {
            let line = &buf[start..start + pos];
            self.line(line.strip_suffix(b"\r").unwrap_or(line), &mut done)?;
            start += pos + 1;
        }
        self.tail = buf[start..].to_vec();
        if self.tail.len() > MAX_BYTES || self.data.len() > MAX_BYTES {
            return Err(too_large());
        }
        Ok(done)
    }

    /// Flush an event whose final blank line never arrived.
    pub fn finish(&mut self) -> Result<Option<String>, Error> {
        let mut done = Vec::new();
        if !self.tail.is_empty() {
            let tail = std::mem::take(&mut self.tail);
            let line = tail.strip_suffix(b"\r").unwrap_or(&tail).to_vec();
            self.line(&line, &mut done)?;
        }
        if done.is_empty() && self.has_data {
            self.has_data = false;
            done.push(std::mem::take(&mut self.data));
        }
        Ok(done.into_iter().next())
    }

    fn line(&mut self, line: &[u8], done: &mut Vec<String>) -> Result<(), Error> {
        if line.is_empty() {
            if self.has_data {
                self.has_data = false;
                done.push(std::mem::take(&mut self.data));
            }
            return Ok(());
        }
        let value = match line.strip_prefix(b"data:") {
            Some(value) => value.strip_prefix(b" ").unwrap_or(value),
            None if line == b"data" => &[][..],
            // Comments and other fields carry nothing we use.
            None => return Ok(()),
        };
        if self.has_data {
            self.data.push('\n');
        }
        self.data.push_str(&String::from_utf8_lossy(value));
        self.has_data = true;
        if self.data.len() > MAX_BYTES {
            return Err(too_large());
        }
        Ok(())
    }
}

fn too_large() -> Error {
    Error::bad_gateway("SSE event exceeds 8 MiB")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_events_across_chunk_boundaries() {
        let mut parser = SseParser::new();
        assert_eq!(
            parser.push(b"event: response.created\nda").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            parser.push(b"ta: {\"a\":1}\n\n").unwrap(),
            vec!["{\"a\":1}".to_owned()]
        );
        assert_eq!(
            parser.push(b"data: [DONE]\n\n").unwrap(),
            vec!["[DONE]".to_owned()]
        );
    }

    #[test]
    fn joins_multiple_data_lines_and_ignores_fields() {
        let mut parser = SseParser::new();
        let events = parser
            .push(b": ping\nid: 4\ndata: one\ndata: two\n\ndata:x\r\n\r\n")
            .unwrap();
        assert_eq!(events, vec!["one\ntwo".to_owned(), "x".to_owned()]);
    }

    #[test]
    fn keeps_partial_tail_until_completed() {
        let mut parser = SseParser::new();
        for byte in b"data: {\"text\":\"hi\"}\n\ndata: tail" {
            let events = parser.push(&[*byte]).unwrap();
            if !events.is_empty() {
                assert_eq!(events, vec!["{\"text\":\"hi\"}".to_owned()]);
            }
        }
        assert_eq!(parser.push(b"\n\n").unwrap(), vec!["tail".to_owned()]);
    }

    #[test]
    fn finish_flushes_event_without_trailing_blank_line() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: last").unwrap().is_empty());
        assert_eq!(parser.finish().unwrap(), Some("last".to_owned()));
        assert_eq!(parser.finish().unwrap(), None);
    }

    #[test]
    fn finish_returns_nothing_for_empty_parser() {
        assert_eq!(SseParser::new().finish().unwrap(), None);
    }

    #[test]
    fn rejects_oversized_tail_and_event() {
        let mut parser = SseParser::new();
        assert!(parser.push(&vec![b'x'; MAX_BYTES + 1]).is_err());

        let mut parser = SseParser::new();
        let line = format!("data: {}\n", "x".repeat(1024 * 1024));
        let mut error = None;
        for _ in 0..9 {
            if let Err(caught) = parser.push(line.as_bytes()) {
                error = Some(caught);
                break;
            }
        }
        assert_eq!(error.unwrap().status, 502);
    }
}
