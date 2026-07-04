//! # dadhichi-dap
//!
//! A **Debug Adapter Protocol client** — the service behind the IDE's debugger.
//! It launches or attaches a debug adapter (`debugpy`, `lldb-dap`,
//! `codelldb`, …), sets breakpoints, controls execution, and inspects threads,
//! forwarding adapter events (`stopped`, `terminated`, …) onto the kernel bus as
//! `dap.<event>` so the debugger UI is just another bus subscriber.
//!
//! ```no_run
//! use dadhichi_dap::DapClient;
//!
//! # async fn demo() -> Result<(), dadhichi_dap::DapError> {
//! let client = DapClient::connect_stdio("lldb-dap", &[], None).await?;
//! client.initialize("lldb").await?;
//! client.set_breakpoints("src/main.rs", &[10, 20]).await?;
//! client.configuration_done().await?;
//! # Ok(())
//! # }
//! ```

pub mod client;
pub mod codec;

pub use client::{DapClient, DapError};
