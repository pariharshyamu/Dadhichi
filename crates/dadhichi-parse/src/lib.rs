//! # dadhichi-parse
//!
//! Tree-sitter-backed extraction of [`Symbol`]s and [`Reference`]s from source
//! code. The indexer feeds files here on every change; the resulting symbols
//! power go-to-definition, outline, and symbol search, while the references form
//! the edges of the call/reference graph.
//!
//! The current grammar is Rust. Adding a language is additive: implement
//! [`LanguageParser`] over another tree-sitter grammar — the rest of the
//! pipeline is language-agnostic.
//!
//! ```
//! use dadhichi_parse::{LanguageParser, RustParser};
//! use std::path::Path;
//!
//! let parsed = RustParser::new()
//!     .parse_all("fn helper() {}\nfn main() { helper(); }", Path::new("a.rs"));
//! assert!(parsed.symbols.iter().any(|s| s.name == "main"));
//! // `main` calls `helper`.
//! assert!(parsed
//!     .references
//!     .iter()
//!     .any(|r| r.to == "helper" && r.from.as_deref() == Some("main")));
//! ```

use dadhichi_workspace::{Reference, Symbol, SymbolKind};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tree_sitter::{Node, Parser};

/// The full result of analysing one file.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Parsed {
    /// Definition symbols found in the file.
    pub symbols: Vec<Symbol>,
    /// Reference edges (call/use sites) found in the file.
    pub references: Vec<Reference>,
}

/// A language-agnostic source analyser producing symbols and references.
///
/// [`parse_all`](Self::parse_all) is the primary method (one parse, both
/// outputs); [`parse`](Self::parse) and
/// [`parse_references`](Self::parse_references) are convenience projections.
pub trait LanguageParser {
    /// Analyse `source`, returning both symbols and references.
    fn parse_all(&self, source: &str, path: &Path) -> Parsed;

    /// Extract only definition symbols.
    fn parse(&self, source: &str, path: &Path) -> Vec<Symbol> {
        self.parse_all(source, path).symbols
    }

    /// Extract only reference edges.
    fn parse_references(&self, source: &str, path: &Path) -> Vec<Reference> {
        self.parse_all(source, path).references
    }
}

/// A Rust source analyser built on the `tree-sitter-rust` grammar.
#[derive(Debug, Default)]
pub struct RustParser;

impl RustParser {
    /// Create a Rust parser.
    pub fn new() -> Self {
        Self
    }
}

/// Map a tree-sitter Rust node kind to a [`SymbolKind`], or `None` if the node
/// is not a definition we index.
fn kind_of(node_kind: &str) -> Option<SymbolKind> {
    Some(match node_kind {
        "function_item" => SymbolKind::Function,
        "struct_item" => SymbolKind::Struct,
        "enum_item" => SymbolKind::Enum,
        "trait_item" => SymbolKind::Trait,
        "mod_item" => SymbolKind::Module,
        "const_item" | "static_item" => SymbolKind::Constant,
        "type_item" => SymbolKind::Struct,
        _ => return None,
    })
}

impl LanguageParser for RustParser {
    fn parse_all(&self, source: &str, path: &Path) -> Parsed {
        let mut parser = Parser::new();
        if parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .is_err()
        {
            return Parsed::default();
        }
        let Some(tree) = parser.parse(source, None) else {
            return Parsed::default();
        };

        let mut parsed = Parsed::default();
        let bytes = source.as_bytes();
        walk(tree.root_node(), None, bytes, path, &mut parsed);
        parsed
    }
}

/// Recursively walk the tree, collecting symbols and references while tracking
/// the nearest enclosing function so references can be attributed to a caller.
///
/// `enclosing` is owned (cloned only at each function boundary, which is rare),
/// sidestepping the borrow conflict of holding a reference into `out` while also
/// mutating `out`.
fn walk(node: Node<'_>, enclosing: Option<String>, bytes: &[u8], path: &Path, out: &mut Parsed) {
    let mut current_enclosing = enclosing;

    // Definitions.
    if let Some(kind) = kind_of(node.kind())
        && let Some(name_node) = node.child_by_field_name("name")
        && let Ok(name) = name_node.utf8_text(bytes)
    {
        out.symbols.push(Symbol {
            name: name.to_string(),
            kind,
            file: path.to_path_buf(),
            // tree-sitter rows are 0-based; editors and LSP are 1-based.
            line: name_node.start_position().row as u32 + 1,
        });
        // References inside a function body are attributed to that function.
        if kind == SymbolKind::Function {
            current_enclosing = Some(name.to_string());
        }
    }

    // References: calls and macro invocations.
    if let Some((to, line)) = reference_target(node, bytes) {
        out.references.push(Reference {
            from: current_enclosing.clone(),
            to,
            file: path.to_path_buf(),
            line,
        });
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, current_enclosing.clone(), bytes, path, out);
    }
}

/// If `node` is a call or macro invocation, return the callee name and its line.
fn reference_target(node: Node<'_>, bytes: &[u8]) -> Option<(String, u32)> {
    match node.kind() {
        "call_expression" => {
            let func = node.child_by_field_name("function")?;
            let name = callee_name(func, bytes)?;
            Some((name, func.start_position().row as u32 + 1))
        }
        "macro_invocation" => {
            let macro_node = node.child_by_field_name("macro")?;
            let name = callee_name(macro_node, bytes)?;
            Some((name, macro_node.start_position().row as u32 + 1))
        }
        _ => None,
    }
}

/// Extract a callee name from the `function`/`macro` node of a call, resolving
/// paths to their final segment and method calls to the method name.
fn callee_name(node: Node<'_>, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node.utf8_text(bytes).ok().map(str::to_string),
        // `path::to::func` — take the final segment.
        "scoped_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string),
        // `receiver.method(...)` — take the method name.
        "field_expression" => node
            .child_by_field_name("field")
            .and_then(|n| n.utf8_text(bytes).ok())
            .map(str::to_string),
        // `func::<T>(...)` — unwrap the generic to its inner function.
        "generic_function" => node
            .child_by_field_name("function")
            .and_then(|n| callee_name(n, bytes)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extracts_top_level_items() {
        let src = r#"
            struct Point { x: i32 }
            enum Color { Red, Green }
            trait Draw { fn draw(&self); }
            const MAX: u32 = 10;
            fn main() {}
        "#;
        let syms = RustParser::new().parse(src, Path::new("lib.rs"));
        let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Point"));
        assert!(names.contains(&"Color"));
        assert!(names.contains(&"Draw"));
        assert!(names.contains(&"MAX"));
        assert!(names.contains(&"main"));
    }

    #[test]
    fn captures_nested_items_with_line_numbers() {
        let src = "mod outer {\n    fn inner() {}\n}\n";
        let syms = RustParser::new().parse(src, Path::new("m.rs"));
        let inner = syms
            .iter()
            .find(|s| s.name == "inner")
            .expect("nested fn found");
        assert_eq!(inner.kind, SymbolKind::Function);
        assert_eq!(inner.line, 2);
        assert!(
            syms.iter()
                .any(|s| s.name == "outer" && s.kind == SymbolKind::Module)
        );
    }

    #[test]
    fn tags_symbols_with_the_given_path() {
        let syms = RustParser::new().parse("fn a() {}", Path::new("src/x.rs"));
        assert_eq!(syms[0].file, PathBuf::from("src/x.rs"));
    }

    #[test]
    fn attributes_calls_to_the_enclosing_function() {
        let src = "fn helper() {}\nfn caller() {\n    helper();\n    println!(\"hi\");\n}\n";
        let parsed = RustParser::new().parse_all(src, Path::new("a.rs"));

        let call = parsed
            .references
            .iter()
            .find(|r| r.to == "helper")
            .expect("call to helper recorded");
        assert_eq!(call.from.as_deref(), Some("caller"));
        assert_eq!(call.line, 3);

        // The macro invocation is captured too, attributed to the same caller.
        assert!(
            parsed
                .references
                .iter()
                .any(|r| r.to == "println" && r.from.as_deref() == Some("caller"))
        );
    }

    #[test]
    fn resolves_paths_and_methods_to_final_segment() {
        let src = "fn f() {\n    std::mem::swap();\n    thing.method();\n}\n";
        let refs = RustParser::new().parse_references(src, Path::new("a.rs"));
        assert!(refs.iter().any(|r| r.to == "swap"));
        assert!(refs.iter().any(|r| r.to == "method"));
    }
}
