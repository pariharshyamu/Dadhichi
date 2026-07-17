//! A minimal, dependency-free HTML structural checker.
//!
//! The HTML language server validates almost nothing structural — it returns an
//! *empty* diagnostic set for unclosed or crossed tags — so the editor runs
//! this checker on open/save and publishes what it finds on the same
//! `lsp.diagnostics` seam the Problems panel already reads.

/// Elements that never take a closing tag.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta",
    "param", "source", "track", "wbr",
];

/// Structural problems in `text`, as `(0-based line, message)` pairs sorted by
/// line: unclosed elements, closing tags that cross other open elements, and
/// closing tags with no opening tag at all.
pub fn diagnostics(text: &str) -> Vec<(u32, String)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut stack: Vec<(String, u32)> = Vec::new();
    let line_of = |pos: usize| text[..pos].bytes().filter(|b| *b == b'\n').count() as u32;

    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // Comments and <!doctype ...> carry no structure.
        if text[i..].starts_with("<!--") {
            i = text[i..].find("-->").map(|p| i + p + 3).unwrap_or(bytes.len());
            continue;
        }
        if text[i..].starts_with("<!") {
            i = text[i..].find('>').map(|p| i + p + 1).unwrap_or(bytes.len());
            continue;
        }
        let closing = text[i..].starts_with("</");
        let name_start = i + if closing { 2 } else { 1 };
        let name: String = text[name_start..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        let Some(gt) = text[i..].find('>') else { break };
        let end = i + gt + 1;
        if name.is_empty() {
            i = end;
            continue;
        }
        let lname = name.to_ascii_lowercase();
        let self_closed = text[i..end].ends_with("/>");

        if closing {
            match stack.iter().rposition(|(n, _)| *n == lname) {
                Some(pos) => {
                    // Everything opened above the match was never closed.
                    for (n, l) in stack.drain(pos + 1..) {
                        out.push((l, format!("unclosed <{n}> — </{lname}> arrived before it was closed")));
                    }
                    stack.pop();
                }
                None => out.push((line_of(i), format!("</{lname}> has no matching opening tag"))),
            }
        } else if !self_closed && !VOID.contains(&lname.as_str()) {
            // Raw-text elements: skip to the matching close so a `<div` inside
            // a script string doesn't confuse the stack.
            if lname == "script" || lname == "style" {
                let close = format!("</{lname}");
                if let Some(p) = text[end..].to_ascii_lowercase().find(&close) {
                    let after = end + p;
                    i = text[after..].find('>').map(|q| after + q + 1).unwrap_or(bytes.len());
                    continue;
                }
                out.push((line_of(i), format!("unclosed <{lname}>")));
                break;
            }
            stack.push((lname, line_of(i)));
        }
        i = end;
    }
    for (n, l) in stack {
        out.push((l, format!("unclosed <{n}>")));
    }
    out.sort_by_key(|(l, _)| *l);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_document_has_no_problems() {
        let html = "<!doctype html>\n<html>\n<body>\n<div><p>hi</p><br><img src=\"x\">\n</div>\n</body>\n</html>\n";
        assert!(diagnostics(html).is_empty());
    }

    #[test]
    fn unclosed_and_crossed_tags_are_reported_with_lines() {
        // </span> never opened; <p> left open when </div> closes; <section> open at EOF.
        let html = "<div>\n<p>hello</span>\n</div>\n<section>\n";
        let probs = diagnostics(html);
        let messages: Vec<&str> = probs.iter().map(|(_, m)| m.as_str()).collect();
        assert!(messages.iter().any(|m| m.contains("</span> has no matching")), "{messages:?}");
        assert!(messages.iter().any(|m| m.contains("unclosed <p>")), "{messages:?}");
        assert!(messages.iter().any(|m| m.contains("unclosed <section>")), "{messages:?}");
        // The unclosed <section> points at its own line (0-based line 3).
        let section = probs.iter().find(|(_, m)| m.contains("<section>")).unwrap();
        assert_eq!(section.0, 3);
    }

    #[test]
    fn script_content_comments_and_self_closing_are_ignored() {
        let html = "<script>if (a < b) { document.write(\"<div>\"); }</script>\n\
                    <!-- <div> in a comment -->\n\
                    <svg/><input>\n";
        assert!(diagnostics(html).is_empty());
    }

    #[test]
    fn an_unclosed_script_is_reported() {
        let probs = diagnostics("<script>let x = 1;\n");
        assert_eq!(probs.len(), 1);
        assert!(probs[0].1.contains("unclosed <script>"));
    }
}
