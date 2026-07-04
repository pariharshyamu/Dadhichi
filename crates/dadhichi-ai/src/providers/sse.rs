//! A minimal Server-Sent Events decoder.
//!
//! Both the OpenAI and Anthropic streaming APIs frame their deltas as SSE: a
//! sequence of `data: <json>` lines. This decoder is a pure byte-in / payloads-
//! out state machine — it holds a buffer across chunk boundaries so a JSON
//! payload split across two network reads is reassembled correctly. Keeping it
//! transport-free makes it exhaustively unit-testable without a socket.

/// Accumulates raw bytes and yields complete SSE `data:` payloads.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: String,
}

impl SseDecoder {
    /// Create an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of bytes, returning every complete `data:` payload it
    /// completes. The terminal `[DONE]` sentinel is filtered out. A payload that
    /// is not yet terminated by a newline stays buffered for the next call.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut out = Vec::new();

        // Process every complete line (terminated by '\n') in the buffer.
        while let Some(nl) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=nl).collect();
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(payload) = line.strip_prefix("data:") {
                let payload = payload.trim();
                if payload.is_empty() || payload == "[DONE]" {
                    continue;
                }
                out.push(payload.to_string());
            }
            // `event:`, comments (`:`), and blank lines are ignored — the caller
            // discriminates on the JSON payload instead.
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_simple_events() {
        let mut d = SseDecoder::new();
        let out = d.feed(b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n");
        assert_eq!(out, vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()]);
    }

    #[test]
    fn reassembles_payload_split_across_chunks() {
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: {\"hel").is_empty());
        let out = d.feed(b"lo\":true}\n");
        assert_eq!(out, vec!["{\"hello\":true}".to_string()]);
    }

    #[test]
    fn filters_done_sentinel_and_named_events() {
        let mut d = SseDecoder::new();
        let out = d.feed(b"event: message\ndata: {\"x\":1}\ndata: [DONE]\n");
        assert_eq!(out, vec!["{\"x\":1}".to_string()]);
    }
}
