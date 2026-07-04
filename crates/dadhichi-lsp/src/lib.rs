//! # dadhichi-lsp
//!
//! A **Language Server Protocol client**. It brings real, language-server-backed
//! code intelligence — hover, go-to-definition, references, and live
//! diagnostics — to the IDE, complementing the tree-sitter index
//! ([`dadhichi-parse`](https://docs.rs)) which supplies fast, offline symbols.
//!
//! The [`LspClient`] is transport-generic: [`connect_stdio`](LspClient::connect_stdio)
//! launches a real server (rust-analyzer, `pyright`, `gopls`, …) over stdio,
//! while tests drive it over an in-memory pipe. Diagnostics the server pushes
//! are republished on the kernel event bus as `lsp.diagnostics` events, so the
//! Problems panel is just another bus subscriber.
//!
//! ```no_run
//! use dadhichi_lsp::{LspClient, Position};
//!
//! # async fn demo() -> Result<(), dadhichi_lsp::LspError> {
//! let client = LspClient::connect_stdio("rust-analyzer", &[], None).await?;
//! client.initialize("file:///my/project").await?;
//! client.did_open("file:///my/project/src/main.rs", "rust", "fn main() {}").await?;
//! let hover = client.hover("file:///my/project/src/main.rs", Position::new(0, 3)).await?;
//! println!("{hover:?}");
//! # Ok(())
//! # }
//! ```

pub mod client;
pub mod codec;
pub mod protocol;

pub use client::{LspClient, LspError};
pub use protocol::{Diagnostic, Location, Position, PublishDiagnostics, Range, Severity};
