//! LSP base-protocol framing.
//!
//! LSP messages are JSON-RPC bodies prefixed with HTTP-style headers, the only
//! required one being `Content-Length`. [`encode`] frames a value; [`LspDecoder`]
//! is a byte-in / messages-out state machine that reassembles frames split
//! across reads. Both are pure and transport-free, so the wire format is fully
//! unit-testable without a language server.

/// Frame a JSON value as an LSP message (header + body).
pub fn encode(message: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(message).unwrap_or_default();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

/// Reassembles LSP frames from a byte stream.
#[derive(Debug, Default)]
pub struct LspDecoder {
    buffer: Vec<u8>,
}

impl LspDecoder {
    /// Create an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes, returning every complete message they finish. A partial
    /// frame (header or body not fully arrived) stays buffered.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<serde_json::Value> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();

        loop {
            let Some(header_end) = find(&self.buffer, b"\r\n\r\n") else {
                break; // header incomplete
            };
            let Some(len) = content_length(&self.buffer[..header_end]) else {
                // Malformed header — discard it and resynchronise.
                self.buffer.drain(..header_end + 4);
                continue;
            };
            let body_start = header_end + 4;
            if self.buffer.len() < body_start + len {
                break; // body incomplete
            }
            let body: Vec<u8> = self.buffer[body_start..body_start + len].to_vec();
            self.buffer.drain(..body_start + len);
            if let Ok(value) = serde_json::from_slice(&body) {
                out.push(value);
            }
        }
        out
    }
}

/// Find the first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Parse the `Content-Length` value from a header block.
fn content_length(header: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(header).ok()?;
    for line in text.split("\r\n") {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            return value.trim().parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_message() {
        let msg = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" });
        let bytes = encode(&msg);
        let mut decoder = LspDecoder::new();
        let out = decoder.feed(&bytes);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["method"], "initialize");
    }

    #[test]
    fn reassembles_frame_split_across_reads() {
        let msg = serde_json::json!({ "id": 7, "result": { "ok": true } });
        let bytes = encode(&msg);
        let (a, b) = bytes.split_at(bytes.len() / 2);

        let mut decoder = LspDecoder::new();
        assert!(decoder.feed(a).is_empty());
        let out = decoder.feed(b);
        assert_eq!(out[0]["id"], 7);
    }

    #[test]
    fn decodes_multiple_frames_in_one_chunk() {
        let mut bytes = encode(&serde_json::json!({ "id": 1 }));
        bytes.extend(encode(&serde_json::json!({ "id": 2 })));
        let mut decoder = LspDecoder::new();
        let out = decoder.feed(&bytes);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1]["id"], 2);
    }
}
