//! A minimal, typed subset of the Language Server Protocol.
//!
//! Only the pieces Dadhichi's code-intelligence surface needs today are modelled
//! — positions, locations, hover, references, and diagnostics. The full
//! protocol is large; this subset keeps the client honest and the JSON typed at
//! the edges without pulling in a heavyweight LSP dependency.

use serde::{Deserialize, Serialize};

/// A zero-based line/character position in a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// Zero-based line.
    pub line: u32,
    /// Zero-based UTF-16 character offset.
    pub character: u32,
}

impl Position {
    /// Construct a position.
    pub fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

/// A range between two [`Position`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    /// Inclusive start.
    pub start: Position,
    /// Exclusive end.
    pub end: Position,
}

/// A location: a range within a document identified by URI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    /// The document URI (e.g. `file:///path/to/x.rs`).
    pub uri: String,
    /// The range within that document.
    pub range: Range,
}

/// The severity of a [`Diagnostic`], matching LSP's numeric scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// A hard error.
    Error,
    /// A warning.
    Warning,
    /// Informational.
    Information,
    /// A hint.
    Hint,
}

impl Severity {
    /// Map the LSP numeric severity (1..=4) onto the enum.
    pub fn from_lsp(n: u8) -> Self {
        match n {
            1 => Severity::Error,
            2 => Severity::Warning,
            3 => Severity::Information,
            _ => Severity::Hint,
        }
    }
}

/// A single diagnostic reported by the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Where the problem is.
    pub range: Range,
    /// How serious it is.
    pub severity: Severity,
    /// The human-readable message.
    pub message: String,
}

/// The payload of a `textDocument/publishDiagnostics` notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishDiagnostics {
    /// The affected document.
    pub uri: String,
    /// The current set of diagnostics for it.
    pub diagnostics: Vec<Diagnostic>,
}

/// Build `initialize` params rooted at `root_uri`.
pub fn initialize_params(root_uri: &str) -> serde_json::Value {
    serde_json::json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "capabilities": {
            "textDocument": {
                "hover": { "contentFormat": ["markdown", "plaintext"] },
                "definition": {},
                "references": {},
                "publishDiagnostics": {}
            }
        }
    })
}

/// Build `textDocument/didOpen` params.
pub fn did_open_params(uri: &str, language_id: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "textDocument": {
            "uri": uri,
            "languageId": language_id,
            "version": 1,
            "text": text
        }
    })
}

/// Build params for a position-based request (`hover`, `definition`, …).
pub fn text_document_position(uri: &str, position: Position) -> serde_json::Value {
    serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": position.line, "character": position.character }
    })
}

/// Build `textDocument/references` params (always requesting the declaration).
pub fn references_params(uri: &str, position: Position) -> serde_json::Value {
    serde_json::json!({
        "textDocument": { "uri": uri },
        "position": { "line": position.line, "character": position.character },
        "context": { "includeDeclaration": true }
    })
}

/// Extract plain hover text from a `textDocument/hover` result, whose
/// `contents` may be a string, a `MarkupContent`, or an array of either.
pub fn parse_hover(result: &serde_json::Value) -> Option<String> {
    let contents = result.get("contents")?;
    Some(flatten_hover(contents)).filter(|s| !s.is_empty())
}

fn flatten_hover(contents: &serde_json::Value) -> String {
    match contents {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => map
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(flatten_hover)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Parse a `definition`/`references` result, which may be a single [`Location`]
/// or an array of them.
pub fn parse_locations(result: &serde_json::Value) -> Vec<Location> {
    match result {
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect(),
        serde_json::Value::Null => Vec::new(),
        single => serde_json::from_value(single.clone()).into_iter().collect(),
    }
}

/// Parse a `publishDiagnostics` notification's params.
pub fn parse_diagnostics(params: &serde_json::Value) -> Option<PublishDiagnostics> {
    let uri = params.get("uri")?.as_str()?.to_string();
    let diagnostics = params
        .get("diagnostics")?
        .as_array()?
        .iter()
        .filter_map(|d| {
            Some(Diagnostic {
                range: serde_json::from_value(d.get("range")?.clone()).ok()?,
                severity: Severity::from_lsp(
                    d.get("severity").and_then(|s| s.as_u64()).unwrap_or(1) as u8,
                ),
                message: d.get("message")?.as_str()?.to_string(),
            })
        })
        .collect();
    Some(PublishDiagnostics { uri, diagnostics })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_handles_markup_and_arrays() {
        assert_eq!(
            parse_hover(&serde_json::json!({ "contents": "hi" })).as_deref(),
            Some("hi")
        );
        assert_eq!(
            parse_hover(
                &serde_json::json!({ "contents": { "kind": "markdown", "value": "**x**" } })
            )
            .as_deref(),
            Some("**x**")
        );
        assert_eq!(
            parse_hover(&serde_json::json!({ "contents": ["a", { "value": "b" }] })).as_deref(),
            Some("a\nb")
        );
        assert!(parse_hover(&serde_json::json!({ "contents": "" })).is_none());
    }

    #[test]
    fn locations_handle_single_and_array() {
        let single = serde_json::json!({
            "uri": "file:///a.rs",
            "range": { "start": { "line": 1, "character": 2 }, "end": { "line": 1, "character": 5 } }
        });
        assert_eq!(parse_locations(&single).len(), 1);
        assert_eq!(
            parse_locations(&serde_json::json!([single.clone(), single])).len(),
            2
        );
        assert!(parse_locations(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn diagnostics_parse_with_severity() {
        let params = serde_json::json!({
            "uri": "file:///a.rs",
            "diagnostics": [{
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                "severity": 2,
                "message": "unused variable"
            }]
        });
        let pd = parse_diagnostics(&params).unwrap();
        assert_eq!(pd.uri, "file:///a.rs");
        assert_eq!(pd.diagnostics[0].severity, Severity::Warning);
        assert_eq!(pd.diagnostics[0].message, "unused variable");
    }
}
