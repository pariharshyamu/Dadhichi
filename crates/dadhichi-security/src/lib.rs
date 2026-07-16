//! # dadhichi-security
//!
//! The IDE's **security suite**, secure by design:
//!
//! - [`vault`] — a ChaCha20-Poly1305 credential [`Vault`] so API keys are never
//!   stored in the clear.
//! - [`secrets`] — [`scan`](secrets::scan) catches credentials leaking into
//!   text before it is committed, sent to a model, or shared.
//! - [`audit`] — a hash-chained [`AuditLog`] that makes every security action
//!   tamper-evident.
//! - [`injection`] — [`assess`](injection::assess) scores untrusted content for
//!   prompt-injection and [`quarantine`](injection::quarantine) fences it.
//! - [`resolver`] — a [`SecretResolver`] that turns `env:` / `vault:` references
//!   into concrete secrets, so credentials stay out of config files.
//! - [`trust`] — a [`TrustStore`] gating a repository's own executable
//!   automation (hooks, repo-local servers) behind an explicit folder-trust
//!   decision that cascades to subdirectories.
//! - [`sandbox`] — kernel-level filesystem confinement (Landlock on Linux) that
//!   covers shelled-out commands and subagents, not just the built-in FS tools.
//!
//! Together these back the permission prompts, credential handling, and
//! untrusted-content guards the agents and plugins rely on.

pub mod audit;
pub mod injection;
pub mod resolver;
pub mod sandbox;
pub mod secrets;
pub mod trust;
pub mod vault;

pub use audit::{AuditEntry, AuditLog};
pub use injection::{Assessment, assess, quarantine};
pub use resolver::SecretResolver;
pub use sandbox::{
    BuiltinBase, CustomProfile, ResolvedProfile, SandboxError, SandboxProfile, SandboxStatus,
    apply as apply_sandbox, parse_profile, resolve as resolve_sandbox,
};
pub use secrets::{Finding, contains_secret, scan};
pub use trust::TrustStore;
pub use vault::{Vault, VaultData, VaultError};
