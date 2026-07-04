//! DAP base-protocol framing.
//!
//! The Debug Adapter Protocol uses the same `Content-Length`-prefixed framing as
//! LSP. [`encode`] frames a message; [`DapDecoder`] reassembles frames from a
//! byte stream. Both are pure and transport-free, hence unit-testable without a
//! debug adapter.

/// Frame a JSON value as a DAP message (header + body).
pub fn encode(message: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(message).unwrap_or_default();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

/// Reassembles DAP frames from a byte stream.
#[derive(Debug, Default)]
pub struct DapDecoder {
    buffer: Vec<u8>,
}

impl DapDecoder {
    /// Create an empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes, returning every complete message they finish.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<serde_json::Value> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let Some(header_end) = find(&self.buffer, b"\r\n\r\n") else {
                break;
            };
            let Some(len) = content_length(&self.buffer[..header_end]) else {
                self.buffer.drain(..header_end + 4);
                continue;
            };
            let body_start = header_end + 4;
            if self.buffer.len() < body_start + len {
                break;
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

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

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
        let msg = serde_json::json!({ "seq": 1, "type": "request", "command": "initialize" });
        let mut decoder = DapDecoder::new();
        let out = decoder.feed(&encode(&msg));
        assert_eq!(out[0]["command"], "initialize");
    }

    #[test]
    fn reassembles_split_frame() {
        let bytes = encode(&serde_json::json!({ "seq": 2, "type": "response" }));
        let (a, b) = bytes.split_at(bytes.len() / 2);
        let mut decoder = DapDecoder::new();
        assert!(decoder.feed(a).is_empty());
        assert_eq!(decoder.feed(b)[0]["seq"], 2);
    }
}
