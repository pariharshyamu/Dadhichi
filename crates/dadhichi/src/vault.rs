//! `dadhichi vault …` operations: populate the encrypted credential store the
//! MCP connectors read `${vault:NAME}` secrets from.
//!
//! The running IDE only ever *reads* the vault (see the app's `SecretResolver`).
//! Mutation lives here, in a separate short-lived invocation, so the
//! long-running process never holds the ability to rewrite credentials — and the
//! secret value is taken from stdin, never argv, so it stays out of shell
//! history and the process table.

use crate::cli::VaultCommand;
use dadhichi_security::{Vault, VaultData};
use std::path::{Path, PathBuf};

/// Why a vault operation could not complete.
#[derive(Debug)]
pub enum VaultOpError {
    /// `$DADHICHI_VAULT_PASSPHRASE` was unset or empty.
    NoPassphrase,
    /// The existing vault could not be decrypted with the given passphrase.
    WrongPassphrase,
    /// The vault file could not be read or written.
    Io(String),
    /// The vault file was not valid JSON.
    Parse(String),
    /// Encrypting the secret failed.
    Encrypt(String),
}

impl std::fmt::Display for VaultOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPassphrase => write!(
                f,
                "set DADHICHI_VAULT_PASSPHRASE to unlock the credential vault"
            ),
            Self::WrongPassphrase => {
                write!(f, "wrong passphrase for the existing vault")
            }
            Self::Io(e) => write!(f, "{e}"),
            Self::Parse(e) => write!(f, "vault file is not valid JSON: {e}"),
            Self::Encrypt(e) => write!(f, "could not encrypt secret: {e}"),
        }
    }
}

/// The credential-vault path: `$DADHICHI_VAULT`, else `~/.dadhichi/vault.json`.
pub fn default_path() -> Option<PathBuf> {
    let nonempty = |v: std::ffi::OsString| (!v.is_empty()).then_some(v);
    if let Some(explicit) = std::env::var_os("DADHICHI_VAULT").and_then(nonempty) {
        return Some(PathBuf::from(explicit));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .and_then(nonempty)?;
    Some(PathBuf::from(home).join(".dadhichi").join("vault.json"))
}

/// Execute a parsed `vault` subcommand, printing results and exiting non-zero on
/// failure. Reads the passphrase from the environment and the secret from stdin.
pub fn run(cmd: VaultCommand) {
    if matches!(cmd, VaultCommand::Help) {
        println!("{}", help_text());
        return;
    }
    let Some(path) = default_path() else {
        fail("cannot determine the vault path (set DADHICHI_VAULT or HOME)");
    };
    let passphrase = std::env::var("DADHICHI_VAULT_PASSPHRASE").unwrap_or_default();

    let outcome = match cmd {
        VaultCommand::Help => unreachable!("handled above"),
        VaultCommand::List => list(&path, &passphrase).map(print_names),
        VaultCommand::Set { name } => {
            let secret = read_secret_from_stdin();
            set(&path, &passphrase, &name, &secret).map(|()| println!("stored `{name}`"))
        }
        VaultCommand::Remove { name } => remove(&path, &passphrase, &name).map(|existed| {
            if existed {
                println!("removed `{name}`");
            } else {
                println!("no secret named `{name}`");
            }
        }),
    };
    if let Err(e) = outcome {
        fail(&e.to_string());
    }
}

fn print_names(names: Vec<String>) {
    if names.is_empty() {
        println!("(vault is empty)");
    } else {
        for name in names {
            println!("{name}");
        }
    }
}

fn fail(message: &str) -> ! {
    eprintln!("vault: {message}");
    std::process::exit(1);
}

fn help_text() -> String {
    "USAGE:
    dadhichi vault set NAME       Store a secret under NAME (value read from stdin).
    dadhichi vault list           List stored secret names (values stay encrypted).
    dadhichi vault remove NAME    Delete the secret under NAME.

The vault is encrypted with ChaCha20-Poly1305 under DADHICHI_VAULT_PASSPHRASE and
stored at $DADHICHI_VAULT (default ~/.dadhichi/vault.json). MCP servers in
mcp.json reference entries as `${vault:NAME}`.

    export DADHICHI_VAULT_PASSPHRASE='…'
    printf %s \"$TOKEN\" | dadhichi vault set github"
        .to_string()
}

fn read_secret_from_stdin() -> String {
    use std::io::Read;
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    buf.trim_end_matches(['\n', '\r']).to_string()
}

/// Store `secret` under `name`, creating the vault if absent.
pub fn set(path: &Path, passphrase: &str, name: &str, secret: &str) -> Result<(), VaultOpError> {
    require_passphrase(passphrase)?;
    let mut vault = load(path, passphrase)?;
    vault
        .put(name, secret)
        .map_err(|e| VaultOpError::Encrypt(e.to_string()))?;
    save(path, &vault)
}

/// Remove `name`, returning whether it existed. Rewrites the file only if so.
pub fn remove(path: &Path, passphrase: &str, name: &str) -> Result<bool, VaultOpError> {
    require_passphrase(passphrase)?;
    let mut vault = load(path, passphrase)?;
    let existed = vault.remove(name);
    if existed {
        save(path, &vault)?;
    }
    Ok(existed)
}

/// The stored secret names (values stay encrypted). Empty if the vault is absent.
pub fn list(path: &Path, passphrase: &str) -> Result<Vec<String>, VaultOpError> {
    require_passphrase(passphrase)?;
    Ok(load(path, passphrase)?.names())
}

fn require_passphrase(passphrase: &str) -> Result<(), VaultOpError> {
    if passphrase.is_empty() {
        return Err(VaultOpError::NoPassphrase);
    }
    Ok(())
}

/// Open the vault at `path`, or start a fresh one if the file does not exist.
/// Verifies the passphrase against an existing entry so a `set` under the wrong
/// passphrase can't silently add an entry keyed differently from the rest.
fn load(path: &Path, passphrase: &str) -> Result<Vault, VaultOpError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let data: VaultData =
                serde_json::from_str(&text).map_err(|e| VaultOpError::Parse(e.to_string()))?;
            let vault = Vault::with_data(passphrase, data);
            if let Some(name) = vault.names().first() {
                vault.get(name).map_err(|_| VaultOpError::WrongPassphrase)?;
            }
            Ok(vault)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vault::new(passphrase)),
        Err(e) => Err(VaultOpError::Io(e.to_string())),
    }
}

fn save(path: &Path, vault: &Vault) -> Result<(), VaultOpError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| VaultOpError::Io(e.to_string()))?;
    }
    let json = serde_json::to_string_pretty(vault.data())
        .map_err(|e| VaultOpError::Parse(e.to_string()))?;
    std::fs::write(path, json).map_err(|e| VaultOpError::Io(e.to_string()))?;
    restrict_permissions(path);
    Ok(())
}

/// Tighten the vault file to owner-only on Unix; a best-effort no-op elsewhere.
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("vault.json");
        (dir, path)
    }

    #[test]
    fn set_persists_an_encrypted_secret() {
        let (_dir, path) = temp_vault();
        set(&path, "pw", "github", "ghp_token").unwrap();

        // The file exists and does not contain the plaintext secret.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("ghp_token"));

        // Reopening under the same passphrase decrypts it.
        let data: VaultData = serde_json::from_str(&raw).unwrap();
        let vault = Vault::with_data("pw", data);
        assert_eq!(vault.get("github").unwrap().as_deref(), Some("ghp_token"));
    }

    #[test]
    fn set_then_list_then_remove_round_trips() {
        let (_dir, path) = temp_vault();
        set(&path, "pw", "a", "1").unwrap();
        set(&path, "pw", "b", "2").unwrap();
        assert_eq!(
            list(&path, "pw").unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );

        assert!(remove(&path, "pw", "a").unwrap());
        assert!(!remove(&path, "pw", "missing").unwrap());
        assert_eq!(list(&path, "pw").unwrap(), vec!["b".to_string()]);
    }

    #[test]
    fn wrong_passphrase_is_refused_before_corrupting_the_vault() {
        let (_dir, path) = temp_vault();
        set(&path, "right", "a", "1").unwrap();

        // A second `set` under a different passphrase must not append a
        // differently-keyed entry.
        let err = set(&path, "wrong", "b", "2").unwrap_err();
        assert!(matches!(err, VaultOpError::WrongPassphrase));
        assert!(matches!(
            list(&path, "wrong").unwrap_err(),
            VaultOpError::WrongPassphrase
        ));

        // The original entry is intact and still the only one.
        assert_eq!(list(&path, "right").unwrap(), vec!["a".to_string()]);
    }

    #[test]
    fn missing_passphrase_is_rejected() {
        let (_dir, path) = temp_vault();
        assert!(matches!(
            set(&path, "", "a", "1").unwrap_err(),
            VaultOpError::NoPassphrase
        ));
    }

    #[test]
    fn list_of_absent_vault_is_empty() {
        let (_dir, path) = temp_vault();
        assert!(list(&path, "pw").unwrap().is_empty());
    }
}
