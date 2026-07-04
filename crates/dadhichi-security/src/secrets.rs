//! Secret detection: flag credentials accidentally present in text.
//!
//! Runs before content is committed, sent to a model, or shown in a shared
//! session, so a leaked API key is caught at the boundary. Detection is by
//! well-known token shapes; it is intentionally conservative (prefix + length)
//! to keep false positives low.

/// A detected secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// What kind of secret it looks like.
    pub kind: &'static str,
    /// Byte offset of the match in the scanned text.
    pub offset: usize,
    /// A redacted preview (first few characters + `…`).
    pub preview: String,
}

/// Scan `text` for likely secrets, returning one [`Finding`] per hit.
pub fn scan(text: &str) -> Vec<Finding> {
    let mut findings = Vec::new();

    // PEM private key blocks.
    if let Some(offset) = text.find("-----BEGIN") {
        if text[offset..].contains("PRIVATE KEY-----") {
            findings.push(Finding {
                kind: "private-key",
                offset,
                preview: "-----BEGIN…".into(),
            });
        }
    }

    for (offset, token) in tokens(text) {
        if let Some(kind) = classify(token) {
            findings.push(Finding {
                kind,
                offset,
                preview: redact(token),
            });
        }
    }
    findings.sort_by_key(|f| f.offset);
    findings.dedup();
    findings
}

/// Whether `text` contains any likely secret.
pub fn contains_secret(text: &str) -> bool {
    !scan(text).is_empty()
}

/// Classify a token by its shape, or `None` if it looks benign.
fn classify(token: &str) -> Option<&'static str> {
    let len = token.len();
    if token.starts_with("AKIA") && len == 20 && token[4..].chars().all(is_upper_alnum) {
        Some("aws-access-key")
    } else if token.starts_with("ghp_") && len >= 20 {
        Some("github-token")
    } else if token.starts_with("github_pat_") {
        Some("github-fine-grained-token")
    } else if token.starts_with("xoxb-") || token.starts_with("xoxp-") {
        Some("slack-token")
    } else if token.starts_with("sk-") && len >= 20 {
        Some("openai-key")
    } else if token.starts_with("AIza") && len >= 35 {
        Some("google-api-key")
    } else {
        None
    }
}

fn is_upper_alnum(c: char) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit()
}

/// Split text into candidate tokens with their byte offsets.
fn tokens(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        let is_tok = c.is_ascii_alphanumeric() || c == '_' || c == '-';
        match (is_tok, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push((s, &text[s..i]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, &text[s..]));
    }
    out
}

fn redact(token: &str) -> String {
    let head: String = token.chars().take(4).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_token_shapes() {
        let text = "let key = \"AKIAIOSFODNN7EXAMPLE\"; let gh = ghp_1234567890abcdefghij;";
        let kinds: Vec<_> = scan(text).into_iter().map(|f| f.kind).collect();
        assert!(kinds.contains(&"aws-access-key"));
        assert!(kinds.contains(&"github-token"));
    }

    #[test]
    fn detects_private_key_block() {
        let text = "-----BEGIN RSA PRIVATE KEY-----\nMIIB...\n-----END RSA PRIVATE KEY-----";
        assert!(scan(text).iter().any(|f| f.kind == "private-key"));
    }

    #[test]
    fn benign_text_is_clean() {
        assert!(!contains_secret(
            "the quick brown fox jumps over the lazy dog"
        ));
        // A short "sk-" word is not flagged (length guard).
        assert!(!contains_secret("sk-1"));
    }

    #[test]
    fn previews_are_redacted() {
        let finding = &scan("token sk-abcdefghijklmnopqrstuvwx")[0];
        assert_eq!(finding.kind, "openai-key");
        assert!(finding.preview.ends_with('…'));
        assert!(!finding.preview.contains("efghij"));
    }
}
