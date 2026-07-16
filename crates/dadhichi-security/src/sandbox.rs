//! OS-level sandboxing.
//!
//! The application-level [`PathJail`](dadhichi_mcp) confines the built-in
//! virtual-filesystem tools, but a shelled-out `cat /etc/shadow` run through the
//! terminal tool sidesteps it. This module closes that hole with **kernel**
//! enforcement: on Linux it applies a [Landlock](https://landlock.io) ruleset to
//! the whole process at startup, so every child process (`bash`, `rg`, a
//! subagent) inherits the same filesystem limits, enforced by the kernel for the
//! life of the process.
//!
//! # Profiles
//!
//! | Profile | FS read | FS write |
//! |---|---|---|
//! | [`Off`](SandboxProfile::Off) | unrestricted | unrestricted |
//! | [`Workspace`](SandboxProfile::Workspace) | everywhere | cwd + `~/.dadhichi` + temp |
//! | [`ReadOnly`](SandboxProfile::ReadOnly) | everywhere | `~/.dadhichi` + temp |
//! | [`Strict`](SandboxProfile::Strict) | cwd + system paths | cwd + `~/.dadhichi` + temp |
//! | [`Custom`](SandboxProfile::Custom) | base + extra | base + extra |
//!
//! # Fail-open vs fail-closed
//!
//! A **built-in** profile that cannot be enforced (a kernel without Landlock —
//! this crate detects that at runtime) yields [`SandboxStatus::Unsupported`]:
//! the caller logs a warning and continues, matching the documented contract. A
//! **custom** profile with a non-empty `deny` list is **fail-closed**: because
//! kernel-enforced path denial needs a mount-namespace bind-over that this
//! increment does not yet implement, [`apply`] refuses rather than run with the
//! denied paths exposed.
//!
//! # Scope of this increment
//!
//! Filesystem scoping only. `restrict_network` is carried on the resolved
//! profile but **not yet enforced** (that needs seccomp / Landlock's network
//! ABI); [`apply`] reports it via [`SandboxStatus`] so callers can be honest
//! about it. macOS Seatbelt is likewise a follow-up — non-Linux targets return
//! [`SandboxStatus::Unsupported`].

use std::path::{Path, PathBuf};
use thiserror::Error;

/// A sandbox profile: a named policy, or a custom one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxProfile {
    /// No sandbox.
    Off,
    /// Read anywhere; write to the workspace, `~/.dadhichi`, and temp dirs.
    Workspace,
    /// Read anywhere; write only to `~/.dadhichi` and temp dirs.
    ReadOnly,
    /// Read the workspace and system paths only; write to the workspace,
    /// `~/.dadhichi`, and temp dirs.
    Strict,
    /// A user-defined profile layered on a built-in base.
    Custom(CustomProfile),
}

/// A custom profile: a base plus extra grants and (kernel-enforced) denials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomProfile {
    /// The built-in profile to inherit from (never `Custom`).
    pub extends: BuiltinBase,
    /// Block network access for child processes. Not yet enforced (see module
    /// docs); carried for forward-compatibility.
    pub restrict_network: bool,
    /// Extra read-only paths.
    pub read_only: Vec<PathBuf>,
    /// Extra read-write paths.
    pub read_write: Vec<PathBuf>,
    /// Paths/globs to kernel-deny. A non-empty list makes [`apply`] fail-closed
    /// until bind-over enforcement lands.
    pub deny: Vec<String>,
}

/// The built-in profiles a custom profile may extend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinBase {
    /// See [`SandboxProfile::Workspace`].
    Workspace,
    /// See [`SandboxProfile::ReadOnly`].
    ReadOnly,
    /// See [`SandboxProfile::Strict`].
    Strict,
}

/// The outcome of applying a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxStatus {
    /// The profile was `Off`; nothing applied.
    Off,
    /// A kernel ruleset is now enforced (fully or partially).
    Enforced {
        /// Whether `restrict_network` was requested (not yet enforced).
        network_requested: bool,
    },
    /// The platform/kernel can't enforce; the process is unconfined. For a
    /// built-in profile the caller should warn and continue (fail-open).
    Unsupported,
}

/// Failure to apply a sandbox.
#[derive(Debug, Error)]
pub enum SandboxError {
    /// A profile name that isn't recognised.
    #[error("unknown sandbox profile: {0}")]
    UnknownProfile(String),
    /// A custom profile requested `deny` paths, which this increment can't
    /// kernel-enforce; refusing to start rather than expose them.
    #[error("custom `deny` paths are not yet kernel-enforced; refusing to start")]
    DenyUnsupported,
    /// The Landlock backend reported an error while building the ruleset.
    #[error("sandbox backend error: {0}")]
    Backend(String),
}

/// The concrete paths a profile grants, after resolving against a workspace and
/// home directory. Pure and side-effect free — this is the tested core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    /// Directories readable (recursively).
    pub read: Vec<PathBuf>,
    /// Directories readable and writable (recursively).
    pub read_write: Vec<PathBuf>,
    /// Whether child network access should be blocked (not yet enforced).
    pub restrict_network: bool,
    /// Kernel-deny paths/globs (fail-closed until enforced).
    pub deny: Vec<String>,
}

/// System directories a `Strict` profile allows reading (so programs and shared
/// libraries can be loaded). Non-existent entries are skipped at apply time.
const SYSTEM_READ: &[&str] = &[
    "/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc", "/opt", "/proc", "/dev", "/sys",
];

/// Writable temp directories granted by every enforcing profile.
const TEMP_DIRS: &[&str] = &["/tmp", "/var/tmp"];

/// Parse a built-in profile name (as used by `--sandbox` / `[sandbox] profile`).
pub fn parse_profile(name: &str) -> Result<SandboxProfile, SandboxError> {
    Ok(match name {
        "off" => SandboxProfile::Off,
        "workspace" => SandboxProfile::Workspace,
        "read-only" | "readonly" => SandboxProfile::ReadOnly,
        "strict" => SandboxProfile::Strict,
        other => return Err(SandboxError::UnknownProfile(other.to_string())),
    })
}

impl SandboxProfile {
    /// The profile's short name.
    pub fn name(&self) -> &str {
        match self {
            SandboxProfile::Off => "off",
            SandboxProfile::Workspace => "workspace",
            SandboxProfile::ReadOnly => "read-only",
            SandboxProfile::Strict => "strict",
            SandboxProfile::Custom(_) => "custom",
        }
    }
}

/// Resolve a profile into concrete read / read-write path sets, given the
/// workspace root and the user's home directory (for `~/.dadhichi`).
pub fn resolve(profile: &SandboxProfile, cwd: &Path, home: Option<&Path>) -> ResolvedProfile {
    let dadhichi = home.map(|h| h.join(".dadhichi"));
    let temp: Vec<PathBuf> = TEMP_DIRS.iter().map(PathBuf::from).collect();
    let root = PathBuf::from("/");

    let write_common = |base: &mut Vec<PathBuf>| {
        base.push(cwd.to_path_buf());
        if let Some(d) = &dadhichi {
            base.push(d.clone());
        }
        base.extend(temp.iter().cloned());
    };

    match profile {
        SandboxProfile::Off => ResolvedProfile {
            read: vec![root.clone()],
            read_write: vec![root],
            restrict_network: false,
            deny: Vec::new(),
        },
        SandboxProfile::Workspace => {
            let mut rw = Vec::new();
            write_common(&mut rw);
            ResolvedProfile {
                read: vec![root],
                read_write: rw,
                restrict_network: false,
                deny: Vec::new(),
            }
        }
        SandboxProfile::ReadOnly => {
            let mut rw = Vec::new();
            if let Some(d) = &dadhichi {
                rw.push(d.clone());
            }
            rw.extend(temp.iter().cloned());
            ResolvedProfile {
                read: vec![root],
                read_write: rw,
                restrict_network: true,
                deny: Vec::new(),
            }
        }
        SandboxProfile::Strict => {
            let mut read: Vec<PathBuf> = vec![cwd.to_path_buf()];
            read.extend(SYSTEM_READ.iter().map(PathBuf::from));
            let mut rw = Vec::new();
            write_common(&mut rw);
            ResolvedProfile {
                read,
                read_write: rw,
                restrict_network: true,
                deny: Vec::new(),
            }
        }
        SandboxProfile::Custom(c) => {
            let base = match c.extends {
                BuiltinBase::Workspace => SandboxProfile::Workspace,
                BuiltinBase::ReadOnly => SandboxProfile::ReadOnly,
                BuiltinBase::Strict => SandboxProfile::Strict,
            };
            let mut resolved = resolve(&base, cwd, home);
            resolved.read.extend(c.read_only.iter().cloned());
            resolved.read_write.extend(c.read_write.iter().cloned());
            resolved.restrict_network |= c.restrict_network;
            resolved.deny.extend(c.deny.iter().cloned());
            resolved
        }
    }
}

/// Apply `profile` to the current process, irreversibly, for its lifetime.
///
/// Returns the [`SandboxStatus`]. A built-in profile on a kernel without
/// Landlock yields [`Unsupported`](SandboxStatus::Unsupported) (fail-open). A
/// custom profile with `deny` paths returns [`SandboxError::DenyUnsupported`]
/// (fail-closed).
pub fn apply(
    profile: &SandboxProfile,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<SandboxStatus, SandboxError> {
    if matches!(profile, SandboxProfile::Off) {
        return Ok(SandboxStatus::Off);
    }
    let resolved = resolve(profile, cwd, home);
    if !resolved.deny.is_empty() {
        return Err(SandboxError::DenyUnsupported);
    }
    enforce(&resolved)
}

#[cfg(target_os = "linux")]
fn enforce(resolved: &ResolvedProfile) -> Result<SandboxStatus, SandboxError> {
    use landlock::{
        ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
        path_beneath_rules,
    };

    // ABI V1 is the original Landlock filesystem ABI (kernel 5.13); best-effort
    // compatibility (the crate default) degrades gracefully on older kernels
    // and reports the result in the ruleset status.
    let abi = ABI::V1;
    let read = AccessFs::from_read(abi);
    let all = AccessFs::from_all(abi);

    // Only grant paths that exist; a missing path would otherwise error the
    // whole ruleset. `~/.dadhichi` and the workspace are expected to exist.
    let existing = |paths: &[PathBuf]| -> Vec<PathBuf> {
        paths.iter().filter(|p| p.exists()).cloned().collect()
    };
    let ro = existing(&resolved.read);
    let rw = existing(&resolved.read_write);

    let map_err = |e: landlock::RulesetError| SandboxError::Backend(e.to_string());

    let status = Ruleset::default()
        .handle_access(all)
        .map_err(map_err)?
        .create()
        .map_err(map_err)?
        .add_rules(path_beneath_rules(&ro, read))
        .map_err(map_err)?
        .add_rules(path_beneath_rules(&rw, all))
        .map_err(map_err)?
        .restrict_self()
        .map_err(map_err)?;

    Ok(match status.ruleset {
        RulesetStatus::FullyEnforced | RulesetStatus::PartiallyEnforced => SandboxStatus::Enforced {
            network_requested: resolved.restrict_network,
        },
        RulesetStatus::NotEnforced => SandboxStatus::Unsupported,
    })
}

#[cfg(not(target_os = "linux"))]
fn enforce(_resolved: &ResolvedProfile) -> Result<SandboxStatus, SandboxError> {
    // macOS Seatbelt and other backends are a follow-up; treat as unsupported
    // (fail-open for built-ins).
    Ok(SandboxStatus::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/dev")
    }
    fn cwd() -> PathBuf {
        PathBuf::from("/home/dev/project")
    }

    #[test]
    fn parse_known_and_unknown() {
        assert_eq!(parse_profile("workspace").unwrap(), SandboxProfile::Workspace);
        assert_eq!(parse_profile("read-only").unwrap(), SandboxProfile::ReadOnly);
        assert_eq!(parse_profile("readonly").unwrap(), SandboxProfile::ReadOnly);
        assert_eq!(parse_profile("strict").unwrap(), SandboxProfile::Strict);
        assert_eq!(parse_profile("off").unwrap(), SandboxProfile::Off);
        assert!(matches!(
            parse_profile("nope"),
            Err(SandboxError::UnknownProfile(_))
        ));
    }

    #[test]
    fn workspace_reads_everywhere_writes_to_workspace() {
        let r = resolve(&SandboxProfile::Workspace, &cwd(), Some(&home()));
        assert_eq!(r.read, vec![PathBuf::from("/")]);
        assert!(r.read_write.contains(&cwd()));
        assert!(r.read_write.contains(&home().join(".dadhichi")));
        assert!(r.read_write.contains(&PathBuf::from("/tmp")));
        assert!(!r.restrict_network);
    }

    #[test]
    fn read_only_denies_workspace_writes() {
        let r = resolve(&SandboxProfile::ReadOnly, &cwd(), Some(&home()));
        assert_eq!(r.read, vec![PathBuf::from("/")]);
        assert!(!r.read_write.contains(&cwd())); // workspace not writable
        assert!(r.read_write.contains(&home().join(".dadhichi")));
        assert!(r.restrict_network);
    }

    #[test]
    fn strict_scopes_reads_to_workspace_and_system() {
        let r = resolve(&SandboxProfile::Strict, &cwd(), Some(&home()));
        assert!(r.read.contains(&cwd()));
        assert!(r.read.contains(&PathBuf::from("/usr")));
        assert!(!r.read.contains(&PathBuf::from("/"))); // NOT everywhere
        assert!(r.read_write.contains(&cwd()));
        assert!(r.restrict_network);
    }

    #[test]
    fn custom_extends_base_and_adds_paths() {
        let custom = SandboxProfile::Custom(CustomProfile {
            extends: BuiltinBase::Workspace,
            restrict_network: true,
            read_only: vec![PathBuf::from("/data")],
            read_write: vec![PathBuf::from("/scratch")],
            deny: vec![],
        });
        let r = resolve(&custom, &cwd(), Some(&home()));
        assert!(r.read.contains(&PathBuf::from("/"))); // from workspace base
        assert!(r.read.contains(&PathBuf::from("/data"))); // extra
        assert!(r.read_write.contains(&PathBuf::from("/scratch"))); // extra
        assert!(r.restrict_network); // custom turned it on
    }

    #[test]
    fn apply_off_is_a_noop() {
        assert_eq!(
            apply(&SandboxProfile::Off, &cwd(), Some(&home())).unwrap(),
            SandboxStatus::Off
        );
    }

    #[test]
    fn apply_custom_deny_is_fail_closed() {
        let custom = SandboxProfile::Custom(CustomProfile {
            extends: BuiltinBase::Strict,
            restrict_network: false,
            read_only: vec![],
            read_write: vec![],
            deny: vec!["**/*.pem".into()],
        });
        // Must refuse rather than silently run with the deny paths exposed.
        assert!(matches!(
            apply(&custom, &cwd(), Some(&home())),
            Err(SandboxError::DenyUnsupported)
        ));
    }
}
