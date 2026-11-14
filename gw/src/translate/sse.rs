//! Incremental server-sent events reader.

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
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(chunk);
        let mut done = Vec::new();
        let mut start = 0;
        while let Some(pos) = buf[start..].iter().position(|&byte| byte == b'\n') {
            let line = &buf[start..start + pos];
            self.line(line.strip_suffix(b"\r").unwrap_or(line), &mut done);
            start += pos + 1;
        }
        self.tail = buf[start..].to_vec();
        done
    }

    fn line(&mut self, line: &[u8], done: &mut Vec<String>) {
        if line.is_empty() {
            if self.has_data {
                self.has_data = false;
                done.push(std::mem::take(&mut self.data));
            }
            return;
        }
        let value = match line.strip_prefix(b"data:") {
            Some(value) => value.strip_prefix(b" ").unwrap_or(value),
            None if line == b"data" => &[][..],
            // Comments and other fields carry nothing we use.
            None => return,
        };
        if self.has_data {
            self.data.push('\n');
        }
        self.data.push_str(&String::from_utf8_lossy(value));
        self.has_data = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_events_across_chunk_boundaries() {
        let mut parser = SseParser::new();
        assert_eq!(
            parser.push(b"event: response.created\nda"),
            Vec::<String>::new()
        );
        assert_eq!(
            parser.push(b"ta: {\"a\":1}\n\n"),
            vec!["{\"a\":1}".to_owned()]
        );
        assert_eq!(parser.push(b"data: [DONE]\n\n"), vec!["[DONE]".to_owned()]);
    }

    #[test]
    fn joins_multiple_data_lines_and_ignores_fields() {
        let mut parser = SseParser::new();
        let events = parser.push(b": ping\nid: 4\ndata: one\ndata: two\n\ndata:x\r\n\r\n");
        assert_eq!(events, vec!["one\ntwo".to_owned(), "x".to_owned()]);
    }

    #[test]
    fn keeps_partial_tail_until_completed() {
        let mut parser = SseParser::new();
        for byte in b"data: {\"text\":\"hi\"}\n\ndata: tail" {
            let events = parser.push(&[*byte]);
            if !events.is_empty() {
                assert_eq!(events, vec!["{\"text\":\"hi\"}".to_owned()]);
            }
        }
        assert_eq!(parser.push(b"\n\n"), vec!["tail".to_owned()]);
    }
}
