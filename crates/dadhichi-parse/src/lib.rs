//! # dadhichi-parse
//!
//! Tree-sitter-backed extraction of [`Symbol`]s from source code. The indexer
//! feeds files here on every change; the resulting symbols are handed to
//! [`SymbolIndex`](dadhichi_workspace::SymbolIndex) to power go-to-definition,
//! outline, breadcrumbs, and symbol search.
//!
//! The current grammar is Rust. Adding a language is additive: implement
//! [`LanguageParser`] over another tree-sitter grammar and register it — the
//! rest of the pipeline is language-agnostic.
//!
//! ```
//! use dadhichi_parse::{LanguageParser, RustParser};
//! use std::path::Path;
//!
//! let symbols = RustParser::new().parse("fn main() {}\nstruct S;", Path::new("a.rs"));
//! assert!(symbols.iter().any(|s| s.name == "main"));
//! assert!(symbols.iter().any(|s| s.name == "S"));
//! ```

use dadhichi_workspace::{Symbol, SymbolKind};
use std::path::Path;
use tree_sitter::{Node, Parser};

/// A language-agnostic source parser producing definition symbols.
///
/// The `&Path` argument (rather than `impl AsRef<Path>`) keeps the trait
/// dyn-compatible so the indexer can hold a `Box<dyn LanguageParser>` chosen at
/// runtime by file extension.
pub trait LanguageParser {
    /// Extract every top-level and nested definition symbol from `source`,
    /// tagging each with `path`.
    fn parse(&self, source: &str, path: &Path) -> Vec<Symbol>;
}

/// A Rust source parser built on the `tree-sitter-rust` grammar.
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
    fn parse(&self, source: &str, path: &Path) -> Vec<Symbol> {
        let mut parser = Parser::new();
        if parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .is_err()
        {
            return Vec::new();
        }
        let Some(tree) = parser.parse(source, None) else {
            return Vec::new();
        };

        let mut symbols = Vec::new();
        let bytes = source.as_bytes();
        // Iterative pre-order walk so deeply nested items (e.g. functions inside
        // modules or impls) are captured without recursion depth limits.
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if let Some(kind) = kind_of(node.kind())
                && let Some(name_node) = node.child_by_field_name("name")
                && let Ok(name) = name_node.utf8_text(bytes)
            {
                symbols.push(Symbol {
                    name: name.to_string(),
                    kind,
                    file: path.to_path_buf(),
                    // tree-sitter rows are 0-based; editors and LSP are 1-based.
                    line: name_node.start_position().row as u32 + 1,
                });
            }
            push_named_children(node, &mut stack);
        }
        symbols
    }
}

fn push_named_children<'a>(node: Node<'a>, stack: &mut Vec<Node<'a>>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        stack.push(child);
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
}
